#[cfg(all(test, feature = "usb-smartcard"))]
mod e2e_test;

#[cfg(all(test, feature = "usb-smartcard"))]
mod llm_failure_test;

#[cfg(all(test, feature = "usb-smartcard"))]
mod connection_bounds_test;
