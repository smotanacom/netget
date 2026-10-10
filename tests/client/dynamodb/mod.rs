#[cfg(all(test, feature = "dynamo"))]
mod e2e_test;

#[cfg(all(test, feature = "dynamodb"))]
mod command_channel_test;

#[cfg(all(test, feature = "dynamodb"))]
mod ambient_credentials_test;
