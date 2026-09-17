#[cfg(all(test, feature = "xmlrpc"))]
mod llm_failure_test;
#[cfg(all(test, feature = "xmlrpc"))]
mod test;

#[cfg(all(test, feature = "xmlrpc"))]
mod connection_bounds_test;
