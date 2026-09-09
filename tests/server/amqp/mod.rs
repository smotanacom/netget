//! AMQP server tests module

#[cfg(all(test, feature = "amqp"))]
mod codec_test;
#[cfg(all(test, feature = "amqp"))]
mod e2e_test;
#[cfg(all(test, feature = "amqp"))]
mod peer_inject_test;
