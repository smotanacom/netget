#[cfg(all(test, feature = "dns"))]
mod bounds_test;
#[cfg(all(test, feature = "dns"))]
mod dig_test;
#[cfg(all(test, feature = "dns"))]
pub mod kdig_test;
#[cfg(all(test, feature = "dns"))]
mod llm_failure_test;
#[cfg(all(test, feature = "dns"))]
mod test;
