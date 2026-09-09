#[cfg(all(test, feature = "mongodb"))]
mod command_channel_test;
// Both e2e_test and command_channel_test drive NetGet's own MongoDB server, which is behind
// the separate `mongodb-server` feature — see the header of each.
#[cfg(all(test, feature = "mongodb", feature = "mongodb-server"))]
pub mod e2e_test;
