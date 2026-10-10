//! SQS client tests

#[cfg(all(test, feature = "sqs"))]
pub mod e2e_test;

#[cfg(all(test, feature = "sqs"))]
mod command_channel_test;

#[cfg(all(test, feature = "sqs"))]
mod ambient_credentials_test;
