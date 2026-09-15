#[cfg(all(test, feature = "npm"))]
mod e2e_test;

#[cfg(all(test, feature = "npm"))]
mod command_channel_test;

#[cfg(all(test, feature = "npm"))]
mod registry_target_test;
