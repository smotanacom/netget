//! CDP protocol tests
//!
//! Two files, and the split is the point:
//!
//! * `codec_test` exercises the **pure** frame codec against literal specification bytes and
//!   against real captured CDP packets. No server, no LLM, no socket.
//! * `e2e_test` drives the whole event → LLM → action → frame path over the declared UDP test
//!   transport, because the real 802.3 transport needs packet-capture privilege.
//!
//! See `tests/server/cdp/CLAUDE.md`.

#[cfg(all(test, feature = "cdp"))]
pub mod codec_test;

#[cfg(all(test, feature = "cdp"))]
pub mod e2e_test;
