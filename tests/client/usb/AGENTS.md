# USB Client Testing Strategy

## Test Approach

USB client testing is **challenging** because it requires actual USB hardware. Unlike network protocols where we can use
netcat or mock servers, USB testing requires:

1. **Physical USB device** connected to the test machine
2. **Known VID/PID** of the test device
3. **Device-specific protocol** knowledge

This makes automated CI/CD testing difficult without dedicated test hardware.

## Testing Strategies

### 1. Manual Testing (Primary)

**Recommended approach** for USB client validation:

```bash
# Find connected USB devices (Linux)
lsusb

# Example output:
# Bus 001 Device 005: ID 1234:5678 Custom USB Device

# Connect to device via NetGet
./cargo-isolated.sh run --no-default-features --features usb
> open_client usb 1234:5678 "Read device descriptor"
```

**Test scenarios**:

- Connect to device and read device descriptor
- Send control transfer to get string descriptors
- Bulk data transfer (if device supports)
- Interrupt transfer (if device supports, e.g., HID)

### 2. Mock USB Device Testing (Future)

Create a software-based USB device using Linux USB Gadget API:

- Requires Linux system with USB gadget support
- Can create virtual USB device that responds to transfers
- Useful for CI/CD automation

**Not implemented yet** due to complexity.

### 3. Unit Testing (Limited)

Test **parsing logic only** without actual USB access:

- Device info parsing (VID:PID:INTERFACE string)
- Hex encoding/decoding
- Action parameter validation

These tests don't require USB devices and can run in CI.

## E2E Test Design

**Status**: Not implemented (requires hardware)

If implemented, the E2E test would:

```rust
#[cfg(all(test, feature = "usb"))]
mod usb_e2e_test {
    // Test requires USB test device connected
    // VID:PID must be hardcoded or provided via env var

    #[tokio::test]
    #[ignore] // Ignored by default, run manually with --ignored
    async fn test_usb_device_descriptor() {
        // Open client to USB device
        // Send get_descriptor control transfer
        // Verify LLM receives device info
        // Verify control response event
    }
}
```

**Why ignored by default**:

- Requires specific USB hardware
- Not portable across test environments
- May fail if device not connected

## LLM Call Budget

If E2E tests are implemented: **Target < 5 LLM calls**

1. Device opened event → LLM → control_transfer action
2. Control response event → LLM → bulk_transfer_in action (if applicable)
3. Bulk data event → LLM → detach_device action

**Rationale**: USB devices have simple request/response patterns, so few LLM calls needed.

## Expected Runtime

- **Unit tests**: < 1 second (no USB access)
- **E2E tests**: 10-30 seconds (with --ignored flag)
    - Device enumeration: 1-2 seconds
    - Device opening: 1-2 seconds
    - LLM calls: 5-10 seconds
    - Transfers: 1-5 seconds

## Known Issues

1. **Platform-specific behavior**:
    - Linux: May need udev rules for permissions
    - macOS: Some device classes require entitlements
    - Windows: Usually works without special setup

2. **Device-specific quirks**:
    - Some devices require specific initialization sequences
    - Transfer timeouts vary by device
    - Endpoint addresses are device-specific

3. **Permission errors**:
    - Tests may fail with "permission denied" if udev rules not configured
    - Running as root is not recommended for security

4. **No mock devices**:
    - Cannot run E2E tests in CI without physical hardware
    - Would require USB gadget setup (complex)

## Testing Recommendations

**For development**:

1. Use a cheap USB device for testing (e.g., Arduino, USB-to-serial adapter)
2. Test manually with NetGet CLI
3. Verify LLM can read device descriptor and perform transfers
4. Test with different device types (HID, bulk, etc.)

**For CI/CD**:

1. Skip USB E2E tests by default (use `#[ignore]`)
2. Only run unit tests (parsing, validation)
3. Document manual testing procedure for release validation

**For production**:

1. Test with target USB devices before deployment
2. Verify permissions and device access
3. Test on target platforms (Linux, macOS, Windows)
4. Handle device disconnection gracefully

## Example Manual Test

```bash
# 1. Build NetGet with USB support
./cargo-isolated.sh build --no-default-features --features usb

# 2. Find USB device VID/PID (Linux/macOS)
lsusb

# 3. Connect to device
./target/debug/netget
> open_client usb 1234:5678 "Read device information and perform control transfer to get manufacturer string"

# 4. Verify LLM:
#    - Receives device opened event
#    - Sends control_transfer to get string descriptor
#    - Receives control response with manufacturer string
#    - Displays decoded string

# 5. Test bulk transfer (if device supports)
> (LLM should automatically send bulk_transfer_in based on device)

# 6. Test detach
> (LLM should detach when done)
```

## Test Coverage

**What's tested**:

- ✅ Device info parsing (VID:PID:INTERFACE)
- ✅ Hex encoding/decoding
- ✅ Action parameter validation
- ⚠️ Device opening (manual only)
- ⚠️ Control transfers (manual only)
- ⚠️ Bulk transfers (manual only)
- ⚠️ Interrupt transfers (manual only)
- ❌ Hotplug events (not supported yet)
- ❌ Isochronous transfers (not supported by nusb)

## Future Improvements

1. **USB Gadget Test Harness**:
    - Create Linux USB gadget that emulates test device
    - Allows automated E2E testing in CI
    - Requires USB gadget-capable Linux system

2. **Device Emulator**:
    - Software-only USB device emulator
    - Faster than physical devices
    - More portable across test environments

3. **Integration with USB Test Devices**:
    - Use commercial USB test devices (e.g., Total Phase Beagle)
    - Provides known-good test device
    - Can be shared across test environments
