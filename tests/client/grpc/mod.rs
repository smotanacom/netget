//! gRPC client E2E tests
#![cfg(all(test, feature = "grpc"))]

pub mod e2e_test;

pub mod command_channel_test;

pub mod streaming_test;
