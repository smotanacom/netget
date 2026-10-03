//! Typed, bounded OpenConfig gNMI protocol boundary.
pub mod actions;
pub mod codec;
pub mod json;
mod runtime;
pub mod semantic;
pub mod tls;
pub mod value;
pub use runtime::spawn;
pub const DEFAULT_TLS: bool = false;
pub const DEFAULT_PORT: u16 = 9339;
pub const DEFAULT_RPC_TIMEOUT_SECS: u64 = 300;
pub mod proto {
    pub mod gnmi_ext {
        include!(concat!(env!("OUT_DIR"), "/gnmi_ext.rs"));
    }
    pub mod gnmi {
        include!(concat!(env!("OUT_DIR"), "/gnmi.rs"));
    }
}
