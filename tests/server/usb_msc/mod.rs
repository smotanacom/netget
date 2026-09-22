#[cfg(all(test, feature = "usb-msc"))]
mod e2e_test;

#[cfg(all(test, feature = "usb-msc"))]
pub mod fat16;

#[cfg(all(test, feature = "usb-msc"))]
mod llm_failure_test;

#[cfg(all(test, feature = "usb-msc"))]
mod guard_test;

#[cfg(all(test, feature = "usb-msc"))]
mod connection_bounds_test;
