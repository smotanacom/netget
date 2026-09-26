#[cfg(all(test, feature = "memcached"))]
mod e2e_test;
#[cfg(all(test, feature = "memcached"))]
mod llm_failure_test;

#[cfg(all(test, feature = "memcached"))]
mod real_client_test;

#[cfg(all(test, feature = "memcached"))]
mod peer_inject_test;

#[cfg(all(test, feature = "memcached"))]
mod connection_bounds_test;

#[cfg(all(test, feature = "memcached"))]
mod inbound_limit_test;
