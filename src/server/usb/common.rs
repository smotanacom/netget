//! USB constant tables shared across the USB device protocols
//!
//! These are the USB specification values that `descriptors.rs` and the
//! individual device implementations build their descriptors and control
//! request handling from.
//!
//! The module is compiled under the `usb-common` feature, which every USB
//! device protocol (`usb-keyboard`, `usb-mouse`, `usb-serial`, `usb-msc`,
//! `usb-fido2`, `usb-smartcard`) enables. USB/IP framing - opcodes, protocol
//! version, device paths, connection state - is the `usbip` crate's job and
//! deliberately has no counterpart here.

/// USB device class codes
pub mod device_class {
    pub const USE_INTERFACE: u8 = 0x00;
    pub const AUDIO: u8 = 0x01;
    pub const COMM: u8 = 0x02; // CDC (Communication Device Class)
    pub const HID: u8 = 0x03; // Human Interface Device
    pub const PHYSICAL: u8 = 0x05;
    pub const IMAGE: u8 = 0x06;
    pub const PRINTER: u8 = 0x07;
    pub const MASS_STORAGE: u8 = 0x08;
    pub const HUB: u8 = 0x09;
    pub const CDC_DATA: u8 = 0x0a;
    pub const SMART_CARD: u8 = 0x0b;
    pub const CONTENT_SECURITY: u8 = 0x0d;
    pub const VIDEO: u8 = 0x0e;
    pub const PERSONAL_HEALTHCARE: u8 = 0x0f;
    pub const AUDIO_VIDEO: u8 = 0x10;
    pub const DIAGNOSTIC: u8 = 0xdc;
    pub const WIRELESS: u8 = 0xe0;
    pub const MISCELLANEOUS: u8 = 0xef;
    pub const APPLICATION_SPECIFIC: u8 = 0xfe;
    pub const VENDOR_SPECIFIC: u8 = 0xff;
}

/// USB descriptor types
pub mod descriptor_type {
    pub const DEVICE: u8 = 0x01;
    pub const CONFIGURATION: u8 = 0x02;
    pub const STRING: u8 = 0x03;
    pub const INTERFACE: u8 = 0x04;
    pub const ENDPOINT: u8 = 0x05;
    pub const DEVICE_QUALIFIER: u8 = 0x06;
    pub const OTHER_SPEED_CONFIGURATION: u8 = 0x07;
    pub const INTERFACE_POWER: u8 = 0x08;
    pub const HID: u8 = 0x21;
    pub const HID_REPORT: u8 = 0x22;
    pub const HID_PHYSICAL: u8 = 0x23;
}

/// USB standard requests (bmRequestType)
pub mod request_type {
    // Direction
    pub const HOST_TO_DEVICE: u8 = 0x00;
    pub const DEVICE_TO_HOST: u8 = 0x80;

    // Type
    pub const STANDARD: u8 = 0x00;
    pub const CLASS: u8 = 0x20;
    pub const VENDOR: u8 = 0x40;

    // Recipient
    pub const DEVICE: u8 = 0x00;
    pub const INTERFACE: u8 = 0x01;
    pub const ENDPOINT: u8 = 0x02;
    pub const OTHER: u8 = 0x03;
}

/// USB standard request codes (bRequest)
pub mod request {
    pub const GET_STATUS: u8 = 0x00;
    pub const CLEAR_FEATURE: u8 = 0x01;
    pub const SET_FEATURE: u8 = 0x03;
    pub const SET_ADDRESS: u8 = 0x05;
    pub const GET_DESCRIPTOR: u8 = 0x06;
    pub const SET_DESCRIPTOR: u8 = 0x07;
    pub const GET_CONFIGURATION: u8 = 0x08;
    pub const SET_CONFIGURATION: u8 = 0x09;
    pub const GET_INTERFACE: u8 = 0x0a;
    pub const SET_INTERFACE: u8 = 0x0b;
    pub const SYNCH_FRAME: u8 = 0x0c;
}

/// HID-specific request codes
pub mod hid_request {
    pub const GET_REPORT: u8 = 0x01;
    pub const GET_IDLE: u8 = 0x02;
    pub const GET_PROTOCOL: u8 = 0x03;
    pub const SET_REPORT: u8 = 0x09;
    pub const SET_IDLE: u8 = 0x0a;
    pub const SET_PROTOCOL: u8 = 0x0b;
}

/// CDC-specific request codes.
///
/// This carried a note saying `usb-serial` "does not implement CDC class control
/// requests yet", which stopped being true some time ago and is worth correcting
/// rather than leaving: `UsbCdcAcmSerialHandler::handle_control`
/// (`src/server/usb/serial/handler.rs`) matches on these constants and answers
/// SET_LINE_CODING, GET_LINE_CODING, SET_CONTROL_LINE_STATE and SEND_BREAK
/// itself. `SEND_ENCAPSULATED_COMMAND` and `GET_ENCAPSULATED_RESPONSE` fall
/// through to the catch-all, which answers empty on purpose — an error would
/// abort the USB/IP session for the whole device, and a host probing an optional
/// request must not unplug the port.
pub mod cdc_request {
    pub const SEND_ENCAPSULATED_COMMAND: u8 = 0x00;
    pub const GET_ENCAPSULATED_RESPONSE: u8 = 0x01;
    pub const SET_LINE_CODING: u8 = 0x20;
    pub const GET_LINE_CODING: u8 = 0x21;
    pub const SET_CONTROL_LINE_STATE: u8 = 0x22;
    pub const SEND_BREAK: u8 = 0x23;
}

/// Endpoint transfer types
pub mod transfer_type {
    pub const CONTROL: u8 = 0;
    pub const ISOCHRONOUS: u8 = 1;
    pub const BULK: u8 = 2;
    pub const INTERRUPT: u8 = 3;
}
