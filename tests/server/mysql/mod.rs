#[cfg(all(test, feature = "mysql"))]
mod connection_stats_test;
#[cfg(all(test, feature = "mysql"))]
mod llm_failure_test;
#[cfg(all(test, feature = "mysql"))]
mod packet_limit_test;
#[cfg(all(test, feature = "mysql"))]
mod prepared_statement_test;
#[cfg(all(test, feature = "mysql"))]
mod real_client_test;
#[cfg(all(test, feature = "mysql"))]
mod test;
