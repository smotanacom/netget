// Every SOCKS5 server test lives in `e2e_test.rs` and is declared exactly once.
//
// It used to be declared twice: `test.rs` held the tests, `e2e_test.rs` was a one-line
// `include!("test.rs")`, and this file declared both. Every test therefore ran twice per
// suite -- once as `server::socks5::test::*`, once as `server::socks5::e2e_test::*` -- each
// copy binding its own ports and spending its own LLM budget, for no extra coverage.
#[cfg(all(test, feature = "socks5"))]
pub mod e2e_test;
