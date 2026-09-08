#[cfg(all(test, feature = "postgresql"))]
mod decoder_panic_test;
#[cfg(all(test, feature = "postgresql"))]
mod extended_query_test;
#[cfg(all(test, feature = "postgresql"))]
mod llm_failure_test;
#[cfg(all(test, feature = "postgresql"))]
mod test;
