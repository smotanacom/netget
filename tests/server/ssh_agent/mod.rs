#[cfg(all(test, feature = "ssh-agent", unix))]
mod e2e_test;

// Wire-format unit tests (no server, no LLM).
#[cfg(all(test, feature = "ssh-agent", unix))]
mod test;

// Executor-level assertions on the two actions that carry key material.
#[cfg(all(test, feature = "ssh-agent", unix))]
mod executor_test;

// The connection cap and both read deadlines, from the peer's side.
#[cfg(all(test, feature = "ssh-agent", unix))]
mod connection_bounds_test;
