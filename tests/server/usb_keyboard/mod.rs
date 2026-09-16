#[cfg(all(test, feature = "usb-keyboard"))]
mod e2e_test;

#[cfg(all(test, feature = "usb-keyboard"))]
mod llm_failure_test;

#[cfg(all(test, feature = "usb-keyboard"))]
mod attach_on_import_test;

#[cfg(all(test, feature = "usb-keyboard"))]
mod connection_cap_test;
