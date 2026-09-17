#[cfg(all(test, feature = "http"))]
mod test;

#[cfg(all(test, feature = "http"))]
mod e2e_scheduled_tasks_test;

#[cfg(all(test, feature = "http"))]
mod failure_semantics_test;

#[cfg(all(test, feature = "http"))]
mod decision_tag_test;

#[cfg(all(test, feature = "http"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "http"))]
pub mod real_client_test;
