#[cfg(all(test, feature = "usb-serial"))]
mod e2e_test;

#[cfg(all(test, feature = "usb-serial"))]
mod llm_failure_test;

#[cfg(all(test, feature = "usb-serial"))]
mod line_coding_test;
