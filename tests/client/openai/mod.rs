#[cfg(all(test, feature = "openai"))]
mod e2e_test;

#[cfg(all(test, feature = "openai"))]
mod command_channel_test;
pub mod endpoint_and_limits_test;
