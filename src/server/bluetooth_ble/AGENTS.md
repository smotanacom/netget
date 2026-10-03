# Bluetooth Low Energy (BLE) GATT Server Implementation

## Library Choice

**ble-peripheral-rust v0.2** - Cross-platform BLE peripheral/server library

### Platform Support

✅ **Windows**: Native WinRT Bluetooth API
✅ **macOS/iOS**: Native CoreBluetooth framework via objc2
✅ **Linux**: BlueZ via D-Bus (using bluer backend)

### Why ble-peripheral-rust?

- ✅ **True cross-platform peripheral mode** - Only Rust library with production backends on all platforms
- ✅ **Clean async API** - Tokio-based with event-driven architecture
- ✅ **Reuses mature libraries** - bluer (Linux), native OS APIs (Windows/macOS)
- ✅ **GATT server support** - Full service/characteristic management
- ⚠️ **Early maturity** (v0.2, ~3K downloads) - Experimental status appropriate
- 📚 Documentation: https://crates.io/crates/ble-peripheral-rust

### Alternatives Considered

| Library   | Peripheral Support | Cross-Platform  | Verdict                      |
|-----------|--------------------|-----------------|------------------------------|
| btleplug  | ❌ Central only     | ✅ Win/Mac/Linux | Wrong role                   |
| bluest    | ❌ Central only     | ✅ Win/Mac/Linux | Wrong role                   |
| bluer     | ✅ Full GATT server | ❌ Linux only    | Platform-limited             |
| btle      | ⚠️ WIP             | ⚠️ Win/Linux    | Not production-ready         |
| bluster   | ✅ Peripheral       | ✅ Mac/Linux     | Abandoned (2+ years)         |
| SimpleBLE | ✅ C++ only         | ✅ All platforms | Rust bindings = central only |

**Conclusion**: ble-peripheral-rust is the only viable cross-platform peripheral option in pure Rust.

### System Dependencies

**Linux (BlueZ)**:

```bash
# Ubuntu/Debian
sudo apt install bluez libdbus-1-dev pkg-config

# Fedora/RHEL
sudo dnf install bluez dbus-devel pkgconf-pkg-config

# Start bluetoothd daemon
sudo systemctl start bluetooth
```

**macOS**:

- macOS Big Sur (11)+ requires app bundle with `Info.plist` containing `NSBluetoothAlwaysUsageDescription` for
  production
- Development: May need manual permission grants or notarization
- No additional system dependencies required

**Windows**:

- Windows 10+ with Bluetooth support
- No additional system dependencies required
- Uses native WinRT Bluetooth LE APIs

## Architecture

### BLE GATT Hierarchy

```
Peripheral (Device)
├── Service (e.g., Heart Rate Service 0x180D)
│   ├── Characteristic (e.g., HR Measurement 0x2A37)
│   │   ├── Properties: [read, notify]
│   │   ├── Permissions: [readable]
│   │   ├── Value: [0x00, 0x48] (72 BPM)
│   │   └── Descriptors: [...]
│   └── Characteristic (e.g., Body Sensor Location 0x2A38)
│       ├── Properties: [read]
│       ├── Value: [0x01] (Chest)
└── Service (Battery Service 0x180F)
    └── Characteristic (Battery Level 0x2A19)
        ├── Properties: [read, notify]
        └── Value: [0x5F] (95%)
```

### LLM Integration Points

The LLM has full control over the BLE GATT server:

#### 1. Server Initialization (Event-triggered)

**Event**: `bluetooth_server_started`

- Triggered when server starts and adapter is powered on
- LLM decides initial services to create
- LLM configures advertising parameters

**LLM Actions**:

```json
{
  "type": "add_service",
  "uuid": "0000180d-0000-1000-8000-00805f9b34fb",
  "primary": true,
  "characteristics": [
    {
      "uuid": "00002a37-0000-1000-8000-00805f9b34fb",
      "properties": ["read", "notify"],
      "permissions": ["readable"],
      "initial_value": "0048"
    }
  ]
}
```

> **macOS caveat — `initial_value` on a non-read-only characteristic is ignored.**
> The example above is the common case and it is *not* portable as written. CoreBluetooth's
> `-[CBMutableCharacteristic initWithType:properties:value:permissions:]` raises
> `NSInvalidArgumentException` when a cached value is combined with anything beyond read-only
> properties and permissions — and that Objective-C exception crosses the FFI boundary, where Rust
> cannot catch it, and **aborts the whole process** ("fatal runtime error: Rust cannot catch
> foreign exceptions"). `ble-peripheral-rust` notes the same constraint in its CoreBluetooth
> backend (`peripheral_manager.rs:205`) but does not enforce it.
>
> `execute_add_service` therefore drops the cached value on Apple targets when the characteristic
> is not strictly read-only, and logs a WARN. Nothing is lost: reads are answered through the
> `bluetooth_read_request` → `respond_to_read` path, never from that cache. Linux/BlueZ and
> Windows/WinRT are unaffected (the guard is `#[cfg(target_vendor = "apple")]`).
>
> To carry an initial value on macOS, declare the characteristic read-only
> (`"properties": ["read"]`, `"permissions": ["readable"]`); otherwise supply the value via
> `respond_to_read` or `send_notification`.

#### 2. Service Management (Async Actions)

- **add_service**: Create new GATT service with characteristics
- **start_advertising**: Begin advertising (make device discoverable)
- **stop_advertising**: Stop advertising

**Example**:

```json
{
  "type": "start_advertising",
  "device_name": "MyHeartRateMonitor"
}
```

#### 3. Read Requests (Sync Actions - Event Response)

**Event**: `bluetooth_read_request`

```json
{
  "event": "bluetooth_read_request",
  "characteristic_uuid": "00002a37-0000-1000-8000-00805f9b34fb",
  "offset": 0
}
```

**LLM Response**:

```json
{
  "type": "respond_to_read",
  "value": "0048"  // Hex-encoded: 72 in decimal
}
```

#### 4. Write Requests (Sync Actions - Event Response)

**Event**: `bluetooth_write_request`

```json
{
  "event": "bluetooth_write_request",
  "characteristic_uuid": "00002a39-0000-1000-8000-00805f9b34fb",
  "value": "01",
  "offset": 0,
  "with_response": true
}
```

**LLM Response**:

```json
{
  "type": "respond_to_write",
  "status": "success"
}
```

#### 5. Notifications (Async Actions)

**Action**: `send_notification`

- Push data to subscribed clients
- Used for periodic updates (heart rate, temperature, etc.)

```json
{
  "type": "send_notification",
  "characteristic_uuid": "00002a37-0000-1000-8000-00805f9b34fb",
  "value": "0049"  // Updated value
}
```

#### 6. Subscription Events

**Event**: `bluetooth_subscribe`

- Client subscribes to or unsubscribes from notifications
- LLM can start/stop periodic updates

```json
{
  "event": "bluetooth_subscribe",
  "characteristic_uuid": "00002a37-0000-1000-8000-00805f9b34fb",
  "subscribed": true
}
```

### State Management

**Server-Level State**:

- `memory`: LLM conversation memory
- `characteristics`: Tracked characteristic metadata and current values, keyed by
  `characteristic_key` (see below)

**There is no per-connection state machine here, and there never was one that worked.** This
module used to carry the `ConnectionState` Idle/Processing/Accumulating trio and a
`queued_events` vector, documented as "prevents concurrent LLM calls" and "events queued during
Processing state". Neither was true. `event_loop` is a `while let Some(event) = event_rx.recv()`
loop that awaits each event's LLM call to completion before receiving the next, so the state was
set to `Processing` and back to `Idle` inside a single iteration and nothing could ever observe
it as anything but `Idle`. The `Processing` and `Accumulating` match arms were unreachable, and
`queued_events` was pushed to in four places and drained in none.

That is worse than merely dead: had the loop ever been made concurrent, those arms would have
moved the ATT `responder` into a vector nothing reads, so the central would wait out its
30-second transaction timeout and tear down the connection rather than get an error. All of it
is removed. **Serialisation here is a property of the loop, not of a state variable** — if
concurrent handling is ever wanted, it needs a real design, not the revival of this.

**Characteristic keys are canonical.** `characteristic_key` normalises every UUID spelling to
the lowercase-hyphenated 128-bit form before it is used as a map key, because the radio reports
`Uuid::to_string()` while the model writes whatever the documentation showed it. Three separate
places disagreed about this before: `add_service` filed values under the model's spelling, the
read/write paths looked them up under the radio's, `send_notification` used the model's again,
and `BleRouter::register_characteristic` merely lowercased. So a service added with this
protocol's own documented `"uuid": "2A37"` had an unreachable `initial_value`, writes never
updated the stored value, and characteristic routing always fell through to the "newest live
server" fallback — which is invisible with one server and cross-wires two.

**No Connection Tracking**:

- BLE peripheral mode is broadcast-based
- Multiple clients can connect, but ble-peripheral-rust abstracts this
- Server responds to all clients uniformly

### Event Flow

```
1. Server Startup:
   Peripheral::new() → Wait for powered on
   → bluetooth_server_started event
   → LLM adds services (add_service actions)
   → LLM starts advertising (start_advertising action)

2. Client Scans & Connects:
   BLE advertising → Client discovers device → Client connects
   (Connection is transparent to server)

3. Client Reads Characteristic:
   ReadRequest event from ble-peripheral-rust
   → State: Idle → Processing
   → bluetooth_read_request event to LLM
   → LLM responds with respond_to_read action
   → Send response via responder
   → State: Processing → Idle

4. Client Subscribes to Notifications:
   SubscribeNotifications event
   → bluetooth_subscribe event to LLM
   → LLM starts periodic send_notification actions

5. Periodic Notifications (Scheduled Tasks):
   schedule_task(interval=2s, send_notification)
   → LLM sends updated values every 2 seconds
   → Subscribed clients receive updates

6. Client Writes Characteristic:
   WriteRequest event with data
   → State: Idle → Processing
   → Store value in characteristic data
   → bluetooth_write_request event to LLM
   → LLM processes write
   → Send acknowledgment if with_response=true
   → State: Processing → Idle
```

### Shared radio (one `Peripheral` per process)

`ble-peripheral-rust`'s CoreBluetooth backend routes **every** `Peripheral` through one
process-global manager thread, guarded by its own `static PERIPHERAL_THREAD: OnceCell<()>`
(`peripheral_manager.rs:50`). Only the *first* `Peripheral::new()` in a process actually spawns
that thread and connects its command channel; every later `Peripheral::new()` gets a fresh
`manager_tx` whose receiver is immediately dropped, so all of its commands — including
`is_powered()` — silently fail. That is why starting a **second** BLE server used to burn ~10s
and then falsely report "Bluetooth adapter failed to power on after 10 seconds": the adapter was
fine, the peripheral was dead. (IMPROVEMENTS item 2.)

Because CoreBluetooth is genuinely one-manager-per-process anyway (one GATT database, one
advertising state, one radio), `spawn_with_llm_actions` no longer creates a `Peripheral` per
start. It acquires a process-wide shared `BleHub` via `BluetoothBle::shared_hub` (a
`tokio::sync::OnceCell`): the `Peripheral` is built and the adapter power-on wait happens **once**,
on the first start; later starts reuse the live radio with no wait and no hang. On failure the
`OnceCell` stays empty, so a later start retries rather than caching the failure.

One shared radio means one shared `PeripheralEvent` stream (`Peripheral::new` takes a single
sender for the whole process). A dispatcher task hands each event to a `BleRouter`, which:

- routes a read/write/subscribe event to the server that added its characteristic
  (`register_characteristic`), falling back to the newest live server if none owns it;
- broadcasts adapter `StateUpdate`s to every live server;
- skips closed channels, so a stopped server never captures traffic.

`BleRouter` holds no `Peripheral` — only channels — specifically so its routing logic is testable
without a Bluetooth adapter. It is `pub` for that reason (like `parse_ble_uuid` and
`run_event_loop_without_radio`), since the project forbids `#[cfg(test)]` modules in `src/`.

**Known limitation:** BLE has no real per-server stop path here (no `register_server_task`), so a
server's channel is not actively removed from the router on close; the router prunes it lazily
once the channel is observed closed. Concurrent BLE servers share the single radio's one GATT
database and advertising state — that is a CoreBluetooth reality, not a NetGet choice.

### Async read fallback (no `block_on`)

When the LLM answers a `bluetooth_read_request` without a `respond_to_read` action, the read falls
back to the characteristic's last stored value. That fallback used to acquire the lock with
`futures::executor::block_on(server_data.lock())` inside a synchronous `unwrap_or_else` closure —
a blocking call on a tokio worker thread, the exact antipattern the root `CLAUDE.md` documents
(its panic is swallowed by the `tokio::spawn` running the event loop, so the server looks healthy
while the read dies). The event loop is already async at that point, so the lock is now simply
`.await`ed. (IMPROVEMENTS item 3.)

### Dual Logging

All BLE events are logged to both `netget.log` (via `tracing`) and the TUI (via `status_tx`):

- **INFO**: Server started, advertising started/stopped, subscriptions
- **DEBUG**: Read/write requests with characteristic UUIDs, notification sends
- **TRACE**: Full hex-encoded data payloads
- **ERROR**: LLM failures, peripheral errors, invalid UUIDs

Example:

```
[INFO] Bluetooth adapter powered on
[INFO] Added BLE service 0000180d-0000-1000-8000-00805f9b34fb with 2 characteristics
[INFO] Started BLE advertising as 'NetGet-HeartRate'
[DEBUG] BLE read request on 00002a37-0000-1000-8000-00805f9b34fb (offset: 0)
[DEBUG] Sent BLE notification on 00002a37-0000-1000-8000-00805f9b34fb (2 bytes)
[TRACE] BLE write data (hex): 01
```

## Data Format

All BLE data exchanged with LLM is **hex-encoded strings**, not raw bytes:

### UUIDs

- **Standard 16-bit**: `"180D"` → expanded to `0000180d-0000-1000-8000-00805f9b34fb`
- **Full 128-bit**: `"0000180d-0000-1000-8000-00805f9b34fb"`

### Values

- **Hex-encoded**: `"0048"` = [0x00, 0x48] = 72 in decimal
- **Leading zeros**: Important for proper byte representation
- **Optional 0x prefix**: `"0x0048"` and `"0048"` both work

**Why hex-encoded?**

- LLMs understand hex better than base64 for small values
- Direct mapping to Bluetooth SIG specifications
- Easy to construct multi-byte values (e.g., heart rate: flags + BPM)

### Example: Heart Rate Measurement

Standard Bluetooth SIG format:

```
Byte 0: Flags (0x00 = uint8 format, no sensor contact)
Byte 1: BPM value (0x48 = 72)
```

LLM constructs:

```json
{
  "value": "0048"  // Flags: 0x00, BPM: 0x48
}
```

## Common GATT Services

LLMs are familiar with standard Bluetooth SIG GATT services:

### Heart Rate Service (0x180D)

- **Characteristic 0x2A37**: Heart Rate Measurement (notify)
    - Byte 0: Flags
    - Byte 1+: BPM value

### Battery Service (0x180F)

- **Characteristic 0x2A19**: Battery Level (read, notify)
    - Single byte: 0-100 percentage

### Device Information (0x180A)

- **Characteristic 0x2A29**: Manufacturer Name (read)
- **Characteristic 0x2A24**: Model Number (read)
- **Characteristic 0x2A26**: Firmware Revision (read)

### Environmental Sensing (0x181A)

- **Characteristic 0x2A6E**: Temperature (read, notify)
- **Characteristic 0x2A6F**: Humidity (read, notify)

Full specifications: https://www.bluetooth.com/specifications/assigned-numbers/

## Limitations

### BLE Only (No Bluetooth Classic)

- Cannot emulate Bluetooth Classic devices (speakers, mice, keyboards)
- Only Bluetooth Low Energy (BLE/Bluetooth 4.0+)
- Use case: IoT sensors, fitness trackers, smart home devices

### Platform-Specific Behavior

**macOS**:

- Verified working on macOS 26 / Apple Silicon: adapter powers on, services register, advertising
  starts, and all three `tests/server/bluetooth_ble` e2e tests pass. See `docs/archive/MACOS_SUPPORT.md` at the
  repo root for the commands and evidence.
- The `libdbus` dependency in the root `CLAUDE.md` table is Linux-only — macOS links
  `CoreBluetooth.framework` and needs no installed package.
- A cached `initial_value` is only legal on a read-only characteristic; see the caveat above.
- Production apps require .app bundle with Info.plist
- Development may need manual permission grants
- Note `tests/server/bluetooth_ble/e2e_test.rs` is gated on **both** `bluetooth-ble` and
  `bluetooth-ble-client`; building with only the former silently runs zero tests.

**Linux**:

- Requires `bluetoothd` daemon running
- User may need to be in `bluetooth` group or use sudo
- BlueZ version 5.50+ recommended

**Windows**:

- Windows 10+ required
- Native Bluetooth LE adapter required (no USB dongles tested)
- Some adapters may have driver issues

### ble-peripheral-rust Maturity

- **Version 0.2.0** (early development)
- Only ~3,000 total downloads
- Limited documentation and examples
- Platform-specific bugs possible

**Mitigation**:

- Mark protocol as `Experimental`
- Comprehensive error logging
- Document known issues as discovered
- Can contribute fixes upstream (small codebase)

### No Connection-Level Control

- ble-peripheral-rust abstracts connection management
- Cannot accept/reject individual client connections
- Cannot distinguish between multiple connected clients
- All clients receive the same data

### Advertising Limitations

- Basic advertising only (device name, service UUIDs)
- No manufacturer data customization
- No beacon protocols (iBeacon, Eddystone) in v0.2

`start_advertising(name, uuids)` takes a local name and a service-UUID list and nothing else, so
iBeacon (manufacturer data) and Eddystone (service data) are both inexpressible. **0.2.0 is the
newest release** — only 0.1.0 and 0.2.0 have ever been published, the latest on 2024-12-28 — so
this is an upstream limit, not a version lag.

`bluetooth_ble_beacon` no longer builds on this stack for that reason. It talks to BlueZ
directly through `bluer` (Linux only), which is where `ManufacturerData` and `ServiceData` live,
and refuses to start elsewhere. It is not a profile wrapper any more and shares no code with
this module — see `src/server/bluetooth_ble_beacon/CLAUDE.md`. If this base ever gains an
advertising-payload API, that is a chance to re-unify them, not a reason to assume they are
still related.

## Error Handling

### Adapter Not Powered On

- The **first** BLE server in the process waits up to 10 seconds for the adapter to power on, in
  `shared_hub`; later starts reuse the already-powered shared radio and do not wait. (This is the
  fix for the second-start hang — see "Shared radio" above.)
- Error if not powered after timeout
- User should check Bluetooth is enabled in system settings

### Invalid UUIDs

- LLM may provide malformed UUIDs
- Server validates and returns error to LLM
- LLM can retry with corrected UUID

### Read/Write Failures

- LLM call failure → return error response to client
- Client sees "Unlikely Error" status
- Logged with ERROR level

### Platform-Specific Errors

- BlueZ D-Bus errors (Linux)
- CoreBluetooth errors (macOS)
- WinRT errors (Windows)
- All logged with full error messages

## Testing Considerations

### Real Hardware Required

- E2E tests need real BLE adapter or simulator
- Simulated BLE devices:
    - **Linux**: `bluetoothctl` with virtual devices
    - **macOS**: Xcode's Bluetooth Simulator (iOS/tvOS targets)
    - **Windows**: Limited simulator options
    - **Cross-platform**: Nordic nRF Connect app for testing

### Test Strategies

1. **Unit tests**: Action parsing, EventType construction (no BLE hardware)
2. **Integration tests**: Mock ble-peripheral-rust if possible
3. **E2E tests**: Real BLE client (nRF Connect, smartphone app, btleplug)

### LLM Call Budget

- E2E tests should minimize LLM calls (< 10 per suite)
- Use scripting mode for predictable sequences
- Test one service type per test case

### CI/CD Challenges

- GitHub Actions runners don't have Bluetooth adapters
- E2E tests must be run locally or on special runners
- Consider marking E2E tests as `#[ignore]` for CI

## Example Usage Scenarios

### Scenario 1: Heart Rate Monitor

```
User: "Act as a BLE heart rate monitor. Start at 72 BPM and increase by 1 every 2 seconds."

LLM Actions:
1. add_service(uuid="180D", characteristics=[{ uuid="2A37", properties=["notify"], ... }])
2. start_advertising(device_name="NetGet-HR")
3. [Client subscribes to notifications]
4. send_notification(uuid="2A37", value="0048")  // 72 BPM
5. [2 seconds later]
6. send_notification(uuid="2A37", value="0049")  // 73 BPM
... continues
```

### Scenario 2: Temperature Sensor

```
User: "Pretend to be a BLE thermometer. Start at 20°C, simulate random variations."

LLM Actions:
1. add_service(uuid="181A", characteristics=[{ uuid="2A6E", properties=["read", "notify"], ... }])
2. start_advertising(device_name="NetGet-Temp")
3. [Client reads characteristic]
4. bluetooth_read_request → respond_to_read(value="0C80")  // 20.0°C in Bluetooth format
5. [Client subscribes]
6. send_notification(uuid="2A6E", value="0C85")  // 20.5°C
```

### Scenario 3: Custom Interactive Device

```
User: "Create a BLE-controlled LED strip. Reading state returns on/off, writing 0x01 turns on."

LLM Actions:
1. add_service(uuid="12345678-1234-5678-1234-567812345678", characteristics=[
     { uuid="...-0001", properties=["read", "write"], ... }  // State
   ])
2. start_advertising(device_name="NetGet-LED")
3. [Client writes 0x01]
4. bluetooth_write_request(value="01") → LLM updates internal state
5. [Client reads state]
6. bluetooth_read_request → respond_to_read(value="01")  // ON
```

## Future Enhancements

- **Manufacturer data**: Custom advertising payload
- **Connection parameters**: Request faster/slower intervals
- **Bonding/pairing**: Secure connections with PIN/passkey
- **Descriptors**: Client Characteristic Configuration Descriptor (CCCD) control
- **MTU negotiation**: Larger data transfers
- **Multiple services**: Dynamic service add/remove
- **Beacon protocols**: iBeacon, Eddystone support

## Known Issues

- ble-peripheral-rust v0.2 is early development, expect bugs
- macOS may require app bundle for full functionality
- Windows adapter compatibility varies
- No connection-level granularity (all clients treated uniformly)
- Advertising customization limited

## References

- ble-peripheral-rust: https://crates.io/crates/ble-peripheral-rust
- Bluetooth SIG GATT specifications: https://www.bluetooth.com/specifications/specs/
- Bluetooth Core Specification: https://www.bluetooth.com/specifications/bluetooth-core-specification/
- BlueZ (Linux): http://www.bluez.org/
- CoreBluetooth (macOS/iOS): https://developer.apple.com/documentation/corebluetooth
- Standard UUIDs: https://www.bluetooth.com/specifications/assigned-numbers/
