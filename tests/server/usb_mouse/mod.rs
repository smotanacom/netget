#[cfg(all(test, feature = "usb-mouse"))]
mod e2e_test;

#[cfg(all(test, feature = "usb-mouse"))]
mod llm_failure_test;

#[cfg(all(test, feature = "usb-mouse"))]
mod connection_bounds_test;
