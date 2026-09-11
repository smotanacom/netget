# USB Keyboard Server Implementation

## Overview

The USB Keyboard server creates a virtual USB HID (Human Interface Device) keyboard using the USB/IP protocol. This
allows an LLM to control keyboard input on a remote system as if a physical keyboard were attached.

## Architecture

### USB/IP Protocol

- **What**: Network protocol that exports USB devices over TCP/IP
- **Why**: Allows creating virtual USB devices without kernel modules on the server side
- **How**: Server exports device via TCP, client imports with `usbip attach` command

```
┌─────────────────┐                    ┌──────────────────┐
│  NetGet Server  │                    │  Linux Client    │
│  (USB/IP)       │ ◄────── TCP ─────► │  (vhci-hcd)      │
│  Port: 3240     │                    │  usbip attach    │
└─────────────────┘                    └──────────────────┘
         │                                      │
         │ Creates virtual                     │ Sees as
         │ USB keyboard                        │ /dev/input/eventX
         ▼                                     ▼
    [HID Descriptors]                    [Real USB Device]
```

### Components

1. **Common Layer** (`src/server/usb/common.rs`)
    - USB/IP protocol constants and helpers
    - Device class codes, descriptor types
    - Request type/code definitions (standard, HID, CDC)
    - Logging utilities (hex dump, setup packet formatting)

2. **Descriptor Builders** (`src/server/usb/descriptors.rs`)
    - Device descriptor (vendor ID, product ID, device class)
    - HID keyboard report descriptor (boot protocol)
    - Configuration descriptor (interface, HID, endpoint)
    - String descriptors (manufacturer, product, serial)
    - Keyboard report structure (modifiers + 6 keys)
    - Character-to-HID-usage mapping

3. **Server Implementation** (`src/server/usb/keyboard/mod.rs`)
    - TCP server for USB/IP connections
    - Connection state machine (Idle/Processing/Accumulating)
    - Per-connection data (memory, LED status)
    - LLM integration hook (called on device attach)

4. **Protocol Actions** (`src/server/usb/keyboard/actions.rs`)
    - Action definitions (type_text, press_key, press_key_combo)
    - Event definitions (attached, detached, led_status)
    - Server trait implementation (spawn, execute_action)
    - Protocol metadata (experimental state, no privileges required)

## Build Requirements

### System Dependencies

The usbip crate requires `libusb-1.0` to be installed:

```bash
# Ubuntu/Debian
sudo apt-get install libusb-1.0-0-dev pkg-config

# Fedora/RHEL
sudo dnf install libusb1-devel pkgconfig

# macOS
brew install libusb pkg-config
```

**Build Command**:

```bash
./cargo-isolated.sh build --no-default-features --features usb-keyboard
```

## Library Choices

### Primary: `usbip` crate (v0.3)

**Repository**: https://github.com/jiegec/usbip
**License**: MIT
**Maturity**: Active development, API not finalized

**Why chosen**:

- Pure Rust implementation of USB/IP protocol
- No root privileges required on server side
- No kernel modules needed on server side
- Works on Linux/macOS/Windows (server)
- Cross-platform client support (Linux via vhci-hcd)

**Capabilities**:

- Device export/import handling
- URB (USB Request Block) processing
- Descriptor management
- Async/await support with tokio

**Limitations**:

- API stability: Marked as "not finalized", may have breaking changes
- Documentation: Relies heavily on examples
- Client requirements: Needs vhci-hcd kernel module and root access

**Alternatives considered**:

- **usb-gadget** crate: Requires root + kernel modules on server
- **Raw Gadget**: No Rust bindings, very low-level
- **usbip-device**: Alpha quality, development-only

## HID Keyboard Protocol

### Report Descriptor (Boot Protocol)

The keyboard implements USB HID boot protocol for maximum compatibility:

```
Byte 0: Modifiers (8 bits: L-Ctrl, L-Shift, L-Alt, L-GUI, R-Ctrl, R-Shift, R-Alt, R-GUI)
Byte 1: Reserved (always 0)
Bytes 2-7: Up to 6 simultaneous key presses (HID usage codes)
```

**Example**: Typing "a" with shift held:

```
[0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]
 ^^^^         ^^^^
 Shift        'a' key (usage 0x04)
```

### HID Usage Codes

Characters are mapped to HID keyboard usage codes (defined in USB HID specification):

- `a-z`: 0x04-0x1d
- `0-9`: 0x27, 0x1e-0x26
- Special keys: Enter (0x28), Escape (0x29), Backspace (0x2a), etc.
- Modifiers: Ctrl, Shift, Alt, GUI (Windows/Command key)

### LED Output Report

Host can set keyboard LEDs (1 byte):

- Bit 0: Num Lock
- Bit 1: Caps Lock
- Bit 2: Scroll Lock
- Bits 3-4: Reserved
- Bits 5-7: Padding

## LLM Integration

### Connection Flow

1. **Client Connects**: Linux host runs `sudo usbip attach -r <server_ip> -p 3240`
2. **Device Import**: USB/IP protocol exports virtual keyboard
3. **LLM Notified**: `usb_keyboard_attached` event sent to LLM with connection ID
4. **LLM Responds**: Can use actions like `type_text`, `press_key`, etc.
5. **URB Processing**: Server translates LLM actions to USB HID reports
6. **Host Reads**: Linux reads HID reports as keyboard input events

### LLM Actions

#### type_text

```json
{
  "type": "type_text",
  "text": "Hello, World!",
  "typing_speed_ms": 50
}
```

Converts text to sequence of HID reports with press/release cycles.

#### press_key

```json
{
  "type": "press_key",
  "key": "c",
  "modifiers": ["ctrl"]
}
```

Sends single keypress with optional modifiers (Ctrl+C).

#### press_key_combo

```json
{
  "type": "press_key_combo",
  "keys": ["ctrl", "alt", "delete"]
}
```

Presses multiple keys simultaneously (Ctrl+Alt+Delete).

#### release_all_keys

```json
{
  "type": "release_all_keys"
}
```

Releases all currently pressed keys (emergency reset).

### LLM Events

#### usb_keyboard_attached

Triggered when Linux host imports the device.

```json
{
  "type": "usb_keyboard_attached",
  "connection_id": "conn_123"
}
```

#### usb_keyboard_led_status

Triggered when host changes LED state (Caps Lock, Num Lock, etc.).

```json
{
  "type": "usb_keyboard_led_status",
  "connection_id": "conn_123",
  "num_lock": false,
  "caps_lock": true,
  "scroll_lock": false
}
```

## Current status: Experimental, and now actually exercised

### What was broken

**1. Every action failed at runtime.** Actions demanded `action["connection_id"].as_u64()`, while
every event reports `connection_id.to_string()` — which is `"conn-2"`, not `2`. A model quoting
the event's own field back got *"USB keyboard actions require connection_id field in action"*,
and there was no value it could have sent instead. Every keystroke this protocol was asked for
was dropped. `connection_id` is optional now and all three forms are accepted (number, numeric
string, `conn-N`); with one host attached it is inferred, and with several, omitting it is an
error naming the candidates.

The repair left one half of the problem behind for a while, and it was the dangerous half: the
resolved value went through `ConnectionId::new(id as u32)`, which **wraps**. A model naming
connection 4294967298 therefore got connection *2* — on a keyboard, keystrokes typed into
somebody else's window. It is read at full width and refused now, quoting the value back so the
model's repair loop can see what was wrong. The same cast was present in `usb-mouse`,
`usb-msc` and `usb-serial` and is fixed in all four.

**2. `usb_keyboard_led_status` could never fire.** It was declared, carried the full action
vocabulary, and had no emit site: a host sets keyboard LEDs with a class `SET_REPORT` on the
control endpoint, and the crate's `UsbHidKeyboardHandler` never sees output reports.

**3. The crate panics on any control request it does not know.** Its `handle_urb` control arm
ends in `unimplemented!("hid request {:?}", setup)`, and `handle_urb` runs on a tokio worker
inside the session task. `GET_PROTOCOL`, `GET_IDLE`, `GET_REPORT` — and the very `SET_REPORT`
above — would take the connection down with a panic. Pressing Caps Lock sends one.

**4. Key releases were 6 bytes.** The crate answers a release with `vec![0; 6]`. A boot-protocol
report is **8** bytes (modifier, reserved, six key slots); a short release is a different message
with the same intent, which a strict HID parser reads as malformed rather than as "all keys up".

`handler.rs` (`NetGetKeyboardHandler`) fixes 2, 3 and 4 by wrapping the crate's handler: it keeps
the pending-report queue and the report descriptor, and takes over endpoint 0 and the interrupt
IN state machine. Unknown control requests are answered **empty**, not with an error — an error
aborts the USB/IP session for the whole device, and a host probing an optional request must not
disconnect the keyboard.

### What works

- USB/IP on the accepted socket (`usbip::handler`), so the listen port is whatever the caller
  asked for and multiple instances coexist.
- `type_text`, `press_key`, `press_key_combo` (modifiers plus up to six keys in one report),
  `release_all_keys`.
- All three events emitted: `usb_keyboard_attached`, `usb_keyboard_led_status`,
  `usb_keyboard_detached`.
- LED changes are de-duplicated: an identical repeat raises no event. Hosts re-assert the LED
  byte routinely (X11 does it periodically), and without this the model is woken by a stream of
  identical events.
- `type_text` **refuses** characters the US HID layout cannot produce rather than typing a
  different string; a partial send is indistinguishable from success.
- `typing_speed_ms` paces on a spawned task, not `thread::sleep` — `execute_action` is a
  synchronous trait method on a runtime worker. It is capped at 5 seconds a keystroke, because
  that task is **not** registered with `register_server_task`: an unbounded interval left it
  alive long past the server it belongs to.

### What is not verified

Nothing here has been seen by a kernel HID driver. There is no `vhci-hcd`, no
`/dev/input/eventX`, no `evtest`; macOS has no USB/IP client, so the E2E tests speak USB/IP
directly and decode the reports themselves. Also untested: `typing_speed_ms` pacing,
`release_all_keys`, two hosts attached at once, and anything outside the boot protocol (N-key
rollover, multimedia keys).

## Limitations

### Server Side

- **API Instability**: usbip crate API may change (use specific version)
- **Single Device**: One keyboard device per server instance
- **No Hot-Unplug**: Device remains until client detaches
- **Binary Protocol**: LLM cannot directly construct USB/IP messages

### Client Side

- **Linux Only**: Requires vhci-hcd kernel module (Linux 3.17+)
- **Root Access**: Client must run `sudo usbip attach`
- **Manual Import**: User must run attach command (not automatic)
- **No Windows/macOS Client**: Limited to Linux hosts for importing devices

### Protocol

- **Boot Protocol Only**: No advanced HID features (multimedia keys, N-key rollover)
- **6-Key Limit**: Maximum 6 simultaneous non-modifier keys
- **No Latency Guarantee**: Network delays affect typing responsiveness
- **No Device Discovery**: Client must know server IP:port

## Testing Strategy

See `tests/server/usb_keyboard/CLAUDE.md` for E2E testing approach.

**Key Principles**:

- < 10 LLM calls per test suite
- Use real `usbip` client tools
- Test on Linux VM or container
- Verify keyboard events with evtest or similar

## Future Enhancements

### Phase 2: Additional Device Types

- USB Mouse (usb-mouse protocol)
- USB Serial Port (usb-serial protocol, CDC ACM)

### Phase 3: Low-Level Control

- Custom USB devices (usb protocol)
- Full descriptor customization
- Vendor-specific requests

### Advanced Features

- N-key rollover (non-boot protocol)
- Multimedia keys (consumer control)
- LED indicator control
- Multiple simultaneous devices per server

## References

- **USB/IP Protocol**: https://docs.kernel.org/usb/usbip_protocol.html
- **USB HID Specification**: https://www.usb.org/hid
- **USB HID Usage Tables**: https://usb.org/sites/default/files/hut1_4.pdf
- **jiegec/usbip crate**: https://github.com/jiegec/usbip
- **Linux vhci-hcd**: https://docs.kernel.org/usb/usbip_protocol.html#vhci
