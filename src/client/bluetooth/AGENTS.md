# Bluetooth Low Energy (BLE) Client Implementation

## Library Choice

**btleplug v0.11** - Cross-platform Bluetooth Low Energy library for Rust

- ✅ Cross-platform: Windows, macOS, Linux, iOS, Android
- ✅ Actively maintained (Jan 2025 updates)
- ✅ Async Rust with Tokio support
- ✅ Host/central mode (perfect for client implementation)
- ⚠️ **BLE only** - Does NOT support Bluetooth Classic/2
- Documentation: https://docs.rs/btleplug/

### Platform-Specific Backend

btleplug uses native platform APIs:

- **Windows**: Windows Runtime (WinRT) Bluetooth LE APIs
- **macOS/iOS**: CoreBluetooth framework via objc2
- **Linux**: BlueZ via D-Bus
- **Android**: Android Bluetooth LE APIs (requires hybrid Rust/Java build)

### System Dependencies

**Linux (BlueZ)**:
Requires libdbus-1 development files for BlueZ communication:

```bash
# Ubuntu/Debian
sudo apt install libdbus-1-dev pkg-config

# Fedora/RHEL
sudo dnf install dbus-devel pkgconf-pkg-config
```

**macOS**:
On macOS Big Sur (11) or later, applications must be packaged as .app bundles with an Info.plist that includes
`NSBluetoothAlwaysUsageDescription` key to access Bluetooth. For development/testing, the application may need to be
notarized or permissions granted manually.

**Windows**:
No additional system dependencies required.

## Architecture

### BLE Connection Workflow

```
Manager → Adapter → Scan → Peripheral → Connect → Discover Services → Read/Write/Subscribe
```

1. **Manager**: Entry point, provides access to BLE adapters
2. **Adapter**: Represents a Bluetooth adapter, handles scanning
3. **Scan**: Discover nearby BLE devices (5-10 second duration)
4. **Peripheral**: Represents a discovered BLE device
5. **Connect**: Establish connection to peripheral
6. **Discover Services**: Enumerate GATT services and characteristics
7. **Operations**: Read, write, subscribe to characteristic notifications

### LLM Integration Points

The LLM has full control over the BLE client lifecycle:

1. **Scan Phase** (Optional):
    - Action: `scan_devices` with configurable duration
    - Event: `bluetooth_scan_complete` with list of devices (address, name, RSSI)
    - LLM decides which device to connect to

2. **Connection Phase**:
    - Action: `connect_device` by address or name
    - Event: `bluetooth_connected` when connection established
    - Automatic service discovery follows

3. **Service Discovery**:
    - Action: `discover_services`
    - Event: `bluetooth_services_discovered` with GATT hierarchy
    - LLM learns about available services/characteristics

4. **Data Operations**:
    - Action: `read_characteristic` (service UUID + characteristic UUID)
    - Event: `bluetooth_data_read` with value (hex-encoded)
    - Action: `write_characteristic` with value
    - Action: `subscribe_notifications` for async updates
    - Event: `bluetooth_notification_received` when data arrives

5. **Disconnection**:
    - Action: `disconnect`
    - Event: `bluetooth_disconnected`

### State Management

- **ConnectionState**: Idle/Processing/Accumulating (same as TCP client)
- **ClientData**:
    - `peripheral`: Optional handle to connected device
    - `manager`: BLE manager instance
    - `adapter`: BLE adapter instance
    - `memory`: LLM conversation memory
    - `state`: Current connection state

### Notification Handling

btleplug provides async notification callbacks. When a characteristic sends a notification:

1. Callback fires with `ValueNotification` containing UUID and data
2. Event `bluetooth_notification_received` created
3. LLM called with event to decide response actions
4. Actions executed (may include reading more characteristics, writing responses, etc.)

**Note**: btleplug notification callbacks don't include the service UUID, only the characteristic UUID. The LLM must
track which characteristics belong to which services.

## Data Format

All BLE data exchanged with the LLM is **structured JSON**, not raw bytes:

### Device Information

```json
{
  "address": "AA:BB:CC:DD:EE:FF",
  "name": "Heart Rate Monitor",
  "rssi": -65
}
```

### Service/Characteristic Structure

```json
{
  "uuid": "0000180f-0000-1000-8000-00805f9b34fb",
  "primary": true,
  "characteristics": [
    {
      "uuid": "00002a19-0000-1000-8000-00805f9b34fb",
      "properties": ["read", "notify"]
    }
  ]
}
```

### Data Values

```json
{
  "value_hex": "5f",
  "value": "95"  // Human-readable interpretation (when applicable)
}
```

**Why structured data?**

- LLMs cannot effectively parse or construct raw bytes
- Structured data enables semantic understanding (battery level: 95%)
- Standard GATT UUIDs are well-known to LLMs (e.g., 0x180F = Battery Service)
- Actions specify UUIDs as strings, implementation handles parsing

## Common GATT Services

LLMs are familiar with standard Bluetooth SIG GATT services:

- **Battery Service** (0x180F): Battery level (0-100%)
    - Characteristic 0x2A19: Battery Level (read, notify)

- **Heart Rate** (0x180D): Heart rate measurements
    - Characteristic 0x2A37: Heart Rate Measurement (notify)

- **Device Information** (0x180A): Manufacturer, model, firmware
    - Characteristic 0x2A29: Manufacturer Name (read)
    - Characteristic 0x2A24: Model Number (read)
    - Characteristic 0x2A26: Firmware Revision (read)

- **Current Time** (0x1805): Current time synchronization
    - Characteristic 0x2A2B: Current Time (read, notify, write)

Full list: https://www.bluetooth.com/specifications/assigned-numbers/

## Limitations

### BLE Only (No Bluetooth Classic)

- Cannot connect to Bluetooth Classic devices (keyboards, speakers, mice)
- Only Bluetooth Low Energy (BLE/Bluetooth 4.0+) is supported
- Use case: IoT sensors, fitness trackers, smart home devices, beacons

### Platform Permissions

- **macOS**: Requires .app bundle with Info.plist for production use
- **Linux**: May require user to be in `bluetooth` group or use sudo
- **Windows**: Generally works without special permissions
- **Android**: Complex setup with Java integration

### Concurrent Connections

- Most BLE adapters support 7-10 concurrent peripheral connections
- This implementation creates one client instance per connection
- Multiple clients can share the same BLE adapter

### Service Discovery Caching

- btleplug caches service/characteristic discovery results
- Re-connecting to the same device may use cached data
- No manual cache invalidation API

### Notification Registration

- Subscribing to notifications requires the characteristic to support `notify` or `indicate` properties
- Some characteristics require writing to Client Characteristic Configuration Descriptor (CCCD)
- btleplug handles CCCD writes automatically via `subscribe()`

### Write Types

- `WithResponse`: Waits for acknowledgment from device (slower, reliable)
- `WithoutResponse`: Fire-and-forget (faster, may lose data)
- LLM specifies `with_response` parameter (default: true)

## Error Handling

### Connection Errors

- Device not found → LLM receives error, may retry scan
- Connection timeout → Device out of range or powered off
- Connection dropped → `bluetooth_disconnected` event, client enters Disconnected state

### Operation Errors

- Characteristic not found → Service/characteristic UUID mismatch
- Read/write permission denied → Characteristic properties don't allow operation
- Invalid UUID → LLM provided malformed UUID string

### Platform-Specific Errors

- BlueZ errors on Linux (e.g., "org.bluez.Error.Failed")
- CoreBluetooth errors on macOS (e.g., CBErrorDomain)
- WinRT errors on Windows

All errors are logged with `tracing` and reported to LLM as client status updates.

## Testing Considerations

### Real Hardware Required

- E2E tests need a real BLE device or simulator
- Simulated BLE devices:
    - **Linux**: `bluetoothctl` with virtual devices
    - **macOS**: Xcode's Bluetooth Simulator
    - **Cross-platform**: SiliconLabs Bluetooth SDK with simulated GATT servers

### Test Strategies

1. **Unit tests**: Action parsing and EventType construction (no BLE hardware)
2. **Integration tests**: Mock btleplug interfaces (if library supports mocking)
3. **E2E tests**: Real BLE device with known services (battery, heart rate)

### LLM Call Budget

- E2E tests should minimize LLM calls (< 10 per suite)
- Reuse scan results across test cases
- Use scripting mode for predictable test sequences

## Example Usage Scenarios

### Scenario 1: Read Battery Level

```
User: "Scan for BLE devices and read battery level from any device"
LLM:
  1. Action: scan_devices (duration: 5 secs)
  2. Event: bluetooth_scan_complete (3 devices found)
  3. Action: connect_device (address: "AA:BB:CC:DD:EE:FF")
  4. Event: bluetooth_connected
  5. Action: discover_services
  6. Event: bluetooth_services_discovered (Battery Service found)
  7. Action: read_characteristic (service: 0x180F, char: 0x2A19)
  8. Event: bluetooth_data_read (value_hex: "5f", value: "95%")
```

### Scenario 2: Subscribe to Heart Rate Notifications

```
User: "Connect to Heart Rate Monitor and stream heart rate data"
LLM:
  1. Action: connect_device (device_name: "Heart Rate")
  2. Event: bluetooth_connected
  3. Action: discover_services
  4. Event: bluetooth_services_discovered (Heart Rate Service found)
  5. Action: subscribe_notifications (service: 0x180D, char: 0x2A37)
  6. Event: bluetooth_notification_received (HR: 72 bpm)
  7. Event: bluetooth_notification_received (HR: 74 bpm)
  ... (continues as notifications arrive)
```

### Scenario 3: Control Smart Bulb

```
User: "Turn on smart bulb and set brightness to 50%"
LLM:
  1. Action: connect_device (device_name: "Smart Bulb")
  2. Event: bluetooth_connected
  3. Action: discover_services
  4. Event: bluetooth_services_discovered (vendor custom service)
  5. Action: write_characteristic (service: vendor-uuid, char: power-uuid, value_hex: "01")
  6. Action: write_characteristic (service: vendor-uuid, char: brightness-uuid, value_hex: "80")
```

## Future Enhancements

- **Connection parameters**: Request faster/slower connection intervals
- **MTU negotiation**: Request larger Maximum Transmission Unit for faster transfers
- **Bond management**: Store/retrieve bonded devices
- **Advertising data parsing**: Extract manufacturer data from scan results
- **Multi-adapter support**: Select specific BLE adapter when multiple available
- **LE Secure Connections**: Pairing/bonding with security

## Known Issues

- btleplug notification callbacks don't include service UUID (characteristic UUID only)
- macOS requires app bundle for production use
- Some devices disconnect after ~30 seconds of inactivity (device-specific)
- Raspberry Pi Bluetooth adapter may require `bluetoothd` restart after errors

## References

- btleplug documentation: https://docs.rs/btleplug/
- Bluetooth SIG GATT specifications: https://www.bluetooth.com/specifications/specs/
- Bluetooth Core Specification: https://www.bluetooth.com/specifications/bluetooth-core-specification/
- BlueZ (Linux): http://www.bluez.org/
- CoreBluetooth (macOS/iOS): https://developer.apple.com/documentation/corebluetooth

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into the running client. `command_loop` in
`mod.rs` drains the bounded channel and runs each action through `execute_llm_action` — the
same function the scan/connect/notification paths use, so an injected verb raises the same
follow-up events.

Everything already hangs off `Arc<Mutex<ClientData>>` (adapter + peripheral), so the command
task just shares it. The channel is registered **before** the scan/connect task, whose first
step is an LLM call a `*` -> manual rule can park for minutes.

Outcomes:

| action | `ClientSendOutcome` |
|---|---|
| `write_characteristic` (completed GATT write) | `Sent { bytes_sent }` |
| `scan_devices` | `Executed { detail: "scan_devices (Ns): K devices found" }` |
| `connect_device` | `Executed { detail: "connect_device <target>: connected" }` |
| `discover_services` | `Executed { detail: "discover_services: K services" }` |
| `read_characteristic` | `Executed { detail: "read_characteristic <uuid>: N bytes read" }` |
| `subscribe_notifications` / `unsubscribe_notifications` | `Executed { detail: "… subscribed/unsubscribed" }` |
| `disconnect` | `Disconnected` (handle dropped) |
| unknown verb | `Rejected { error }` |
| btleplug error (e.g. no peripheral connected) | the reply is `Err`, surfaced to the caller verbatim |

`write_characteristic` is the one BLE verb that may claim `Sent`: a completed GATT write
really went out on the radio.

Lock discipline: the memory string is cloned out of `ClientData` before every
`call_llm_for_client`. It used to be borrowed across the await, which now that a second task
shares the mutex would stall every injected command for the length of the round-trip.
