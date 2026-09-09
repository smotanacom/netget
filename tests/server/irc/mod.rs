#[cfg(all(test, feature = "irc"))]
mod e2e_test;
#[cfg(all(test, feature = "irc"))]
mod framing_test;
#[cfg(all(test, feature = "irc"))]
mod llm_failure_test;
#[cfg(all(test, feature = "irc"))]
mod peer_inject_test;
