#[cfg(all(test, feature = "memcached"))]
mod in_flight_test;
#[cfg(all(test, feature = "memcached"))]
mod real_server_test;
#[cfg(all(test, feature = "memcached"))]
mod wire_test;
