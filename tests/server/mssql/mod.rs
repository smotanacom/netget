//! MSSQL server tests
#![cfg(all(test, feature = "mssql"))]

mod hostile_input_test;
mod llm_failure_test;
mod test;

#[cfg(all(test, feature = "mssql"))]
mod severity_range_test;
