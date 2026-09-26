#[cfg(all(test, feature = "beanstalkd"))]
mod common;
#[cfg(all(test, feature = "beanstalkd"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "beanstalkd"))]
mod e2e_test;
#[cfg(all(test, feature = "beanstalkd"))]
mod llm_failure_test;
#[cfg(all(test, feature = "beanstalkd"))]
mod peer_inject_test;
#[cfg(all(test, feature = "beanstalkd"))]
mod real_client_test;
#[cfg(all(test, feature = "beanstalkd"))]
mod wire_test;
