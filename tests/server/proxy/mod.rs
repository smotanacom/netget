#[cfg(all(test, feature = "proxy"))]
mod e2e_test;
#[cfg(all(test, feature = "proxy"))]
mod llm_failure_test;

// `test.rs` is the functional suite: it stands up real HTTP/HTTPS target servers and drives
// traffic through the proxy with `reqwest`. `e2e_test.rs` only checks that the server starts.
#[cfg(all(test, feature = "proxy"))]
mod test;

#[cfg(all(test, feature = "proxy"))]
mod status_range_test;
