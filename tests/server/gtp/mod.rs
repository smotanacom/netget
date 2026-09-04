//! GTP-C / GTP-U server tests.
//!
//! `codec_test` pins the wire format against literal, 3GPP-derived octets and needs no
//! server; `e2e_test` drives a real netget process over real UDP sockets.

pub mod codec_test;
pub mod e2e_test;
