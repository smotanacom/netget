//! USB protocol implementations
//!
//! This module provides virtual USB device protocols using USB/IP. Each is its
//! own Cargo feature, and each enables the shared `usb-common` feature that
//! gates `common` (USB constant tables) and `descriptors` (descriptor builders):
//!
//! - usb-keyboard: HID Keyboard device
//! - usb-mouse: HID Mouse device
//! - usb-serial: CDC ACM Serial device
//! - usb-msc: Mass Storage Class device (flash drive/disk)
//! - usb-fido2: FIDO2/U2F Security Key device
//! - usb-smartcard: Smart Card (CCID) device over USB/IP (no external daemon)
//!
//! The separate `usb` feature is the USB *client* (`nusb`) in `src/client/usb`,
//! not a server, and does not build anything in this module.

#[cfg(feature = "usb-common")]
pub mod common;

#[cfg(feature = "usb-common")]
pub mod descriptors;

/// The USB/IP message screen every USB server runs the `usbip` crate behind.
#[cfg(feature = "usb-common")]
pub mod guard;

#[cfg(feature = "usb-keyboard")]
pub mod keyboard;

#[cfg(feature = "usb-mouse")]
pub mod mouse;

#[cfg(feature = "usb-serial")]
pub mod serial;

#[cfg(feature = "usb-msc")]
pub mod msc;

#[cfg(feature = "usb-fido2")]
pub mod fido2;

#[cfg(feature = "usb-smartcard")]
pub mod smartcard;

// Re-export protocol implementations
#[cfg(feature = "usb-keyboard")]
pub use keyboard::UsbKeyboardProtocol;

#[cfg(feature = "usb-mouse")]
pub use mouse::UsbMouseProtocol;

#[cfg(feature = "usb-serial")]
pub use serial::actions::UsbSerialProtocol;

#[cfg(feature = "usb-msc")]
pub use msc::UsbMscProtocol;

#[cfg(feature = "usb-fido2")]
pub use fido2::actions::UsbFido2Protocol;

#[cfg(feature = "usb-smartcard")]
pub use smartcard::actions::UsbSmartCardProtocol;
