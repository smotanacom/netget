//! VRRP (v2/v3) and CARP server tests.
//!
//! Two files with different jobs:
//!
//! * `codec_test` — the packet codec against literal specification bytes, in both directions,
//!   plus SHA-1/HMAC-SHA1 against published vectors and an independent implementation. No
//!   sockets, no LLM, no privilege. This is the part of the protocol that is actually proved.
//! * `e2e_test` — the full advertisement → event → model → action → packet path over the UDP
//!   test transport, the silence-on-LLM-failure guarantee, startup-parameter handling, and
//!   the raw transport's refusal to report success without a raw socket.

#[cfg(all(test, feature = "vrrp"))]
mod codec_test;

#[cfg(all(test, feature = "vrrp"))]
mod e2e_test;
