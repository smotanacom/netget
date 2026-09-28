#[cfg(all(test, feature = "otlp"))]
mod answer_with_test;
#[cfg(all(test, feature = "otlp"))]
mod codec_test;
#[cfg(all(test, feature = "otlp"))]
mod common;
#[cfg(all(test, feature = "otlp"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "otlp"))]
mod e2e_test;
#[cfg(all(test, feature = "otlp"))]
mod llm_failure_test;
#[cfg(all(test, feature = "otlp"))]
mod real_client_test;
