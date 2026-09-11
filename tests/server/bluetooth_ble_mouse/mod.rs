//! BLE HID Mouse tests

#![cfg(all(test, feature = "bluetooth-ble-mouse"))]

mod e2e_test;
mod report_descriptor_test;
