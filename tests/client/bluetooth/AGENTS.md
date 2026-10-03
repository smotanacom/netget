# Bluetooth Client Testing Strategy

## Test Approach

### Challenge: Real Hardware Required

Unlike network protocols that can use localhost or netcat, Bluetooth Low Energy (BLE) requires:

- Real BLE hardware (Bluetooth 4.0+ adapter)
- Real BLE peripheral device (sensor, beacon, development kit)
- OR simulated BLE environment (limited availability)

This makes automated E2E testing challenging in CI/CD environments.

### Testing Pyramid

```
E2E Tests (Manual)     ← Real BLE devices
    ↑
Integration Tests      ← Mock btleplug (if possible)
    ↑
Unit Tests            ← Action parsing, EventType construction
```

## Unit Tests (No LLM, No Hardware)

**Test Scope**: Action execution, EventType construction, UUID parsing

**Test Cases**:

1. **Action Parsing**:
    - `scan_devices` with default duration (5 secs)
    - `scan_devices` with custom duration (10 secs)
    - `connect_device` by address
    - `connect_device` by name
    - `connect_device` error (neither address nor name)
    - `write_characteristic` with hex data, and with invalid hex
    - `disconnect`

   There is **no** unit test for `read_characteristic`, `subscribe_notifications` or
   `unsubscribe_notifications`; this list used to claim all three. The `unit_tests` module in
   `e2e_test.rs` is the authority on what exists.

2. **UUID Validation**: *this section described something NetGet does not do.*
    - `test_uuid_parsing` calls `uuid::Uuid::parse_str` on strings that are **already** in
      128-bit form, so it exercises the `uuid` crate rather than any NetGet code.
    - **The client does not expand the 16-bit shorthand.** `src/client/bluetooth/mod.rs` parses
      every UUID with `Uuid::parse_str` directly, so `"180F"` is rejected, not expanded. The
      expansion helper (`parse_ble_uuid`, covered by `tests/ble_uuid_test.rs`) belongs to the
      **server** side and is gated on the `bluetooth-ble` feature, which this client does not
      enable. Pass full 128-bit UUIDs to every client action.

3. **Hex Data Encoding**:
    - `write_characteristic` hex string → bytes conversion
    - Invalid hex strings → error handling

**LLM Call Budget**: 0 (unit tests, no LLM)

**Runtime**: < 1 second

**Example**:

```rust
#[cfg(all(test, feature = "bluetooth-ble-client"))]
mod tests {
    use super::*;
    use crate::llm::actions::client_trait::Client;

    #[test]
    fn test_scan_devices_action() {
        let protocol = BluetoothClientProtocol::new();
        let action = json!({
            "type": "scan_devices",
            "duration_secs": 10
        });

        let result = protocol.execute_action(action).unwrap();
        // Assert Custom action with duration_secs: 10
    }

    #[test]
    fn test_connect_device_requires_address_or_name() {
        let protocol = BluetoothClientProtocol::new();
        let action = json!({
            "type": "connect_device"
        });

        let result = protocol.execute_action(action);
        assert!(result.is_err());  // Neither address nor name provided
    }

    #[test]
    fn test_write_characteristic_hex_parsing() {
        let protocol = BluetoothClientProtocol::new();
        let action = json!({
            "type": "write_characteristic",
            "service_uuid": "0000180f-0000-1000-8000-00805f9b34fb",
            "characteristic_uuid": "00002a19-0000-1000-8000-00805f9b34fb",
            "value_hex": "ff80",
            "with_response": true
        });

        let result = protocol.execute_action(action).unwrap();
        // Assert Custom action with value_bytes: [255, 128]
    }
}
```

## Integration Tests

**Status**: implemented, in `command_channel_test.rs` — this section used to say "not
implemented (btleplug doesn't provide mocking interface)", which is the wrong conclusion from a
true premise. btleplug indeed cannot be mocked, but the part worth testing does not need it:
`command_channel_follows_the_adapter` drives `ClientForm` into the real client loop and calls
`AppState::send_to_client`, asserting that an unknown verb comes back `Rejected` and that a GATT
read with no peripheral attached **errors rather than reporting success**. That is the injected-
action path (`src/client/command_support.rs`), and it is radio-free because every one of those
outcomes is decided before btleplug is reached.

What still needs hardware is only what actually talks to a peripheral, and those tests are
`#[ignore]`d.

**Future Work**: If btleplug adds mock support or we create our own abstraction layer, we can test:

- Scan lifecycle (start → results → stop)
- Connection lifecycle (connect → discover → operations → disconnect)
- Notification handling (subscribe → receive → unsubscribe)

## E2E Tests (Real Hardware)

**Requirements**:

- Real BLE adapter (built-in laptop Bluetooth or USB dongle)
- Real BLE peripheral device with known services
- Ollama running with suitable model

### Recommended Test Device

**Option 1: Nordic Semiconductor nRF52 Development Kit** ($40-60)

- Programmable BLE peripheral
- Can simulate Battery Service, Heart Rate Service, etc.
- Well-documented, widely available
- Can run custom GATT servers for testing

**Option 2: ESP32 Development Board** ($5-15)

- Supports BLE with Arduino/ESP-IDF
- Easy to program custom GATT services
- Widely available, very cheap

**Option 3: Commercial BLE Sensor** (e.g., Xiaomi Mi Band, fitness tracker)

- Real-world device with standard GATT services
- May require pairing/bonding
- Less predictable behavior

**Option 4: Simulated Device** (Platform-Dependent)

- **Linux**: `bluetoothctl` with `bluetoothd` virtual controllers
- **macOS**: Xcode's Bluetooth Simulator (requires Xcode installation)
- **Windows**: No good simulator available
- **Cross-platform**: SiliconLabs Bluetooth SDK (complex setup)

### E2E Test Scenarios

#### Scenario 1: Scan and Connect (< 2 LLM calls)

```
Test: Scan for BLE devices and verify results

Setup:
  - Ensure test BLE device is powered on and advertising
  - Start NetGet with bluetooth client

Prompt: "Scan for BLE devices for 5 seconds"

LLM Call 1:
  - Action: scan_devices (duration: 5)
  - Event: bluetooth_scan_complete (devices: [...])

Validation:
  - At least 1 device found
  - Test device address appears in results
  - RSSI values are reasonable (-30 to -100 dBm)

Runtime: ~7 seconds (5 sec scan + 2 sec LLM)
```

#### Scenario 2: Read Battery Level (< 5 LLM calls)

```
Test: Connect to device and read Battery Service

Setup:
  - Test device must support Battery Service (0x180F)
  - Battery characteristic (0x2A19) must be readable

Prompt: "Connect to device AA:BB:CC:DD:EE:FF and read battery level"

LLM Call 1:
  - Action: connect_device (device_address: "AA:BB:CC:DD:EE:FF")
  - Event: bluetooth_connected

LLM Call 2:
  - Action: discover_services
  - Event: bluetooth_services_discovered (services: [...])

LLM Call 3:
  - Action: read_characteristic (service_uuid / characteristic_uuid, full 128-bit form)
  - Event: bluetooth_data_read (value_hex: "5f")

Validation:
  - Connection succeeds
  - Battery Service (0x180F) found
  - Battery Level characteristic (0x2A19) read successfully
  - Value is 0-100 (battery percentage)

Runtime: ~10 seconds
```

#### Scenario 3: Subscribe to Notifications (< 6 LLM calls)

```
Test: Subscribe to Heart Rate notifications

Setup:
  - Test device must support Heart Rate Service (0x180D)
  - Heart Rate Measurement characteristic (0x2A37) must support notify

Prompt: "Connect to Heart Rate Monitor and subscribe to heart rate updates"

LLM Call 1: connect_device
LLM Call 2: discover_services
LLM Call 3: subscribe_notifications (service_uuid / characteristic_uuid, full 128-bit form)
LLM Call 4: bluetooth_notification_received (HR data)
LLM Call 5: bluetooth_notification_received (HR data)

Validation:
  - Connection succeeds
  - Subscription succeeds
  - At least 2 notifications received within 10 seconds
  - Notification data is non-empty

Runtime: ~15 seconds
```

### LLM Call Budget

**Total for all E2E tests**: < 10 LLM calls

Strategy to minimize calls:

- Reuse scan results across tests (scan once, connect multiple times)
- Use scripting mode where possible
- Bundle multiple operations in single prompt
- Skip redundant service discovery (cache results)

### Runtime Estimate

- **Unit tests**: < 1 second
- **E2E Scenario 1** (scan): ~7 seconds
- **E2E Scenario 2** (read): ~10 seconds
- **E2E Scenario 3** (notifications): ~15 seconds
- **Total E2E**: ~35 seconds

### Known Test Issues

#### Flaky Tests

- BLE advertising may be intermittent (device sleep mode)
- Connection timeouts on weak signal
- Notification timing varies by device

**Mitigation**:

- Retry connection failures (up to 3 attempts)
- Longer timeouts for weak signal environments
- Use development board with reliable advertising

#### Platform-Specific Issues

- **macOS**: May require notarization for Bluetooth access in tests
- **Linux**: May need `sudo` or `bluetooth` group membership
- **Windows**: Generally works, but Bluetooth drivers vary

#### CI/CD Challenges

- GitHub Actions runners don't have Bluetooth adapters
- **Solution**: Mark E2E tests as `#[ignore]` by default
- **Manual testing**: Run locally with `--include-ignored` flag

### Test Isolation

**Bluetooth adapter is shared resource**:

- Only run one BLE test at a time. **Nothing enforces this** — `--ollama-lock` is parsed and
  read by nothing (`tests/ollama_lock_is_a_noop_test.rs` keeps it that way), so it serializes
  nothing. The hardware tests are `#[ignore]`d precisely because no mechanism arbitrates the
  single adapter; run them with `--test-threads=1 --include-ignored`.
- Disconnect from device after each test
- Clear adapter scan cache between tests (may require adapter restart)

### Test Data Privacy

**No external endpoints**:

- All BLE operations are local (no network traffic)
- LLM only sees device addresses/names (no sensitive data)
- Safe for localhost-only testing

## Test Organization

```
tests/client/bluetooth/
├── CLAUDE.md (this file)
├── mod.rs (declares the submodules — an undeclared file is never compiled)
├── e2e_test.rs (unit_tests module, always run; five #[ignore]d hardware tests)
└── command_channel_test.rs (one always-run test; one #[ignore]d hardware test)
```

## E2E Test Template

```rust
#[cfg(all(test, feature = "bluetooth-ble-client"))]
mod e2e_tests {
    use super::*;

    // Test environment setup
    const TEST_DEVICE_ADDRESS: &str = "AA:BB:CC:DD:EE:FF";  // Replace with real device

    #[tokio::test]
    #[ignore]  // Requires real BLE hardware, run manually
    async fn test_bluetooth_scan() {
        // Setup: Start NetGet with bluetooth client
        // ...

        // Prompt LLM to scan
        // ...

        // Verify: At least 1 device found
        // ...
    }

    #[tokio::test]
    #[ignore]
    async fn test_bluetooth_read_battery() {
        // Requires device with Battery Service (0x180F)
        // ...
    }
}
```

## Manual Testing Checklist

Before release, manually verify:

- [ ] Scan discovers nearby BLE devices
- [ ] Connect by address works
- [ ] Connect by name works
- [ ] Service discovery finds standard GATT services
- [ ] Read characteristic returns valid data
- [ ] Write characteristic succeeds (if device supports)
- [ ] Subscribe to notifications works
- [ ] Notifications arrive and trigger LLM
- [ ] Disconnect cleanly closes connection
- [ ] Error handling (device not found, invalid UUID, etc.)

## Test on Multiple Platforms

- [ ] **Linux**: Ubuntu 22.04+ with BlueZ 5.55+
- [ ] **macOS**: macOS 12+ (Monterey or later)
- [ ] **Windows**: Windows 10+ with Bluetooth 4.0+

## References

- btleplug examples: https://github.com/deviceplug/btleplug/tree/master/examples
- Bluetooth GATT testing tools:
    - **Linux**: `bluetoothctl`, `gatttool`
    - **macOS**: `blueutil`, Xcode's Bluetooth Simulator
    - **Cross-platform**: `nRF Connect` mobile app (device testing)
