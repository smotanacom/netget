# Bluetooth LE GATT Server Test Strategy

## Test Approach

### Radio-free tests (no Bluetooth hardware)

These run under the standard `--test-threads=100`, in milliseconds, and are the ones that
actually gate CI-style verification since GitHub runners have no adapter:

- `llm_failure_test.rs` — an LLM failure yields an ATT error (`UnlikelyError`), never a fabricated
  value or a false write acknowledgement. Drives `BluetoothBle::run_event_loop_without_radio`.
- `read_default_value_test.rs` — **DEFECT 1 (item 3)**: the read fallback that used to
  `futures::executor::block_on(server_data.lock())` now `.await`s on the async path. A read the
  LLM answers without a `respond_to_read` must return `Success` via the fallback rather than
  hang/panic. It also carries
  `a_characteristic_added_by_its_short_uuid_is_found_by_a_read`, which pins the characteristic
  **key** invariant: a characteristic added under the 16-bit shorthand (`"2A37"` — what every
  documented `add_service` example uses) must be found by a read for the canonical form the
  radio reports. That was broken for as long as the protocol existed and no test caught it,
  because every test in this directory used the long form on both sides — the one spelling that
  happened to work. **When you add a test here, vary the UUID spelling deliberately**; agreeing
  with the implementation's accident is how this hid.
- `shared_peripheral_routing_test.rs` — **DEFECT 2 (item 2)**: the `BleRouter` that fans the
  single shared radio's events out to per-server channels — routing by characteristic, broadcast
  of `StateUpdate`, newest-live-server fallback, and skipping a stopped (closed) owner. `BleRouter`
  holds no `Peripheral`, so this needs no adapter.

The **shared-`Peripheral` acquisition** itself (`shared_hub` / the `Peripheral::new` + power-on
wait) is *not* covered here: it needs a real adapter and BLE permission. That path is only
reachable through the `#[ignore]`d E2E tests below (and on a headless CI/dev box it cannot run).

### E2E Tests

**3 test cases**, all requiring real Bluetooth adapters or simulators, all `#[ignore]`d (they
contend for the machine's single adapter under `--test-threads=100`). Also gated on
`bluetooth-ble-client`, so a `--features bluetooth-ble`-only build compiles them to nothing.

## Test Environment Requirements

### Hardware Requirements

- **Real Bluetooth adapter** required (BLE 4.0+ capable)
- USB Bluetooth dongles supported
- Built-in laptop/desktop Bluetooth adapters supported

### Platform-Specific Setup

**Linux**:

```bash
# Ensure BlueZ daemon is running
sudo systemctl start bluetooth
sudo systemctl status bluetooth

# Add user to bluetooth group (optional, may avoid sudo)
sudo usermod -a -G bluetooth $USER

# Verify adapter
hciconfig
# or
bluetoothctl
> list
> show
```

**macOS**:

- Enable Bluetooth in System Preferences
- For production apps: Requires .app bundle with Info.plist
- Development: Manual permission grants may be needed
- No additional software required

**Windows**:

- Enable Bluetooth in Settings
- Windows 10+ required
- Native WinRT API used (no additional drivers)

### CI/CD Considerations

**GitHub Actions**: ❌ Standard runners don't have Bluetooth adapters

- E2E tests must run locally or on special self-hosted runners with Bluetooth
- Consider marking E2E tests as `#[ignore]` for CI
- Alternative: Use Bluetooth simulators (platform-specific)

## Test Cases

### 1. `test_bluetooth_heart_rate_server` (E2E)

**LLM Calls**: 1-2 (initial configuration)

**Runtime**: ~15-20 seconds

- 5s server initialization + advertising
- 5-10s BLE scanning and discovery
- 2-3s connection and service discovery
- 1-2s characteristic read

**What it tests**:

- BLE GATT server creation
- Heart Rate Service (0x180D) implementation
- Characteristic read operations
- Hex-encoded value handling (0x0048 = 72 BPM)
- Advertising with custom device name

**Client**: btleplug (BLE central/client library)

**Validation**:

1. Server starts and advertises as "NetGet-HeartRate"
2. Client discovers device via BLE scan
3. Client connects to peripheral
4. Client discovers Heart Rate Service
5. Client reads Heart Rate Measurement characteristic
6. Value is [0x00, 0x48] (flags + 72 BPM)

**Known Issues**:

- May fail if Bluetooth adapter is busy or in use by another app
- Scanning can be flaky on some platforms
- Test gracefully skips if no adapter available

### 2. `test_bluetooth_battery_service` (E2E)

**LLM Calls**: 1-2 (initial configuration)

**Runtime**: ~15-20 seconds

**What it tests**:

- Battery Service (0x180F) implementation
- Single-byte characteristic reads
- Battery level percentage (95% = 0x5F)
- Standard Bluetooth SIG service

**Client**: btleplug

**Validation**:

1. Server advertises as "NetGet-Battery"
2. Client discovers and connects
3. Client reads Battery Level characteristic
4. Value is [0x5F] (95%)

**Rationale**:

- Tests a different standard GATT service
- Verifies single-byte values (vs multi-byte in heart rate)
- Confirms LLM can handle multiple service types

### 3. `test_bluetooth_ble_startup` (E2E, No Client)

**LLM Calls**: 1-2 (initial configuration)

**Runtime**: ~5 seconds

- 3s server run time
- No BLE client interaction

**What it tests**:

- Server can start without crashes
- Custom UUID service creation
- ASCII string to hex conversion ("TEST" → 0x54455354)
- Server runs stably for multiple seconds

**Client**: None (server-only test)

**Validation**:

1. Server starts successfully
2. Server runs for 3 seconds without errors
3. Server stops cleanly

**Rationale**:

- Doesn't require BLE client (can run on systems without working BLE stack)
- Verifies basic server functionality
- Catches startup crashes and immediate failures

## LLM Call Budget

**Total**: < 10 LLM calls for full test suite

- 3 test cases × 1-2 calls each = 3-6 calls
- All tests use simple, deterministic prompts
- No complex multi-turn conversations

**Optimization**:

- Each test uses a single prompt with clear instructions
- LLM configures services and starts advertising in one go
- No iterative testing or retry loops

## Expected Runtime

**Full suite**: ~40-60 seconds

- Parallel: Not recommended (BLE adapter contention)
- Sequential: 15s + 15s + 5s + overhead

**Per-test breakdown**:

1. Heart rate: ~15-20s
2. Battery: ~15-20s
3. Startup: ~5s

**Optimization notes**:

- BLE scanning is the slowest part (5-10s per test)
- Could optimize with faster scan windows (less reliable)
- Server initialization is consistent (~2-3s)

## Known Issues

### 1. BLE Adapter Availability

**Issue**: Tests fail if no Bluetooth adapter present
**Mitigation**: Tests gracefully skip with warning message

### 2. Scanning Flakiness

**Issue**: BLE device discovery can be unreliable

- Devices may not appear in scan results immediately
- Platform-specific scanning behaviors vary

**Mitigation**:

- 10-second scan timeout per test
- Tests retry peripheral discovery multiple times
- Clear error messages differentiate adapter vs discovery failures

### 3. Platform Differences

**Issue**: ble-peripheral-rust behavior varies by platform

- macOS may require app bundle permissions
- Linux requires bluetoothd running
- Windows adapter driver variations

**Mitigation**:

- Platform-specific error messages
- Test documentation includes setup steps
- Tests skip gracefully on permission errors

### 4. Concurrent Test Execution

**Issue**: BLE adapter can only advertise one device at a time
**Mitigation**: Tests should run sequentially (use `--test-threads=1`)

### 5. ble-peripheral-rust Maturity

**Issue**: Library is v0.2 (early development)

- May have platform-specific bugs
- API may change in future versions

**Mitigation**:

- Version pinned in Cargo.toml
- Tests marked as Experimental
- Comprehensive error logging

## Test Maintenance

### Adding New Test Cases

1. Keep LLM calls < 3 per test
2. Use standard Bluetooth SIG services when possible
3. Include graceful skips for adapter unavailability
4. Test one concept per case (service type, operation, etc.)

### Debugging Failed Tests

1. Check Bluetooth adapter status (`hciconfig`, `bluetoothctl`, System Preferences)
2. Verify bluetoothd running (Linux)
3. Check test output for specific error (adapter vs discovery vs connection)
4. Run single test with `--nocapture` for full logs
5. Try increasing timeouts for slow platforms

### Platform Testing

- **Primary development**: Linux (most stable with BlueZ/bluer)
- **Secondary**: macOS (CoreBluetooth)
- **Tertiary**: Windows (WinRT, least tested)

## Example Test Run

Three things about these commands, each of which the previous version got wrong
and each of which makes the difference between running the suite and silently
running nothing:

- **Both features are needed.** `e2e_test.rs` is gated on `bluetooth-ble` *and*
  `bluetooth-ble-client`, so a `--features bluetooth-ble` build compiles it to
  nothing and reports success having run zero E2E tests.
- **`--test` names a test *target*, not a module.** The target is `server` (the
  file `tests/server.rs`); `server::bluetooth_ble` is a filter passed after
  `--`. `--test bluetooth_ble` makes cargo list the available targets and exit
  without running anything, which reads like a broken suite.
- **The E2E tests are `#[ignore]`d**, so they need `--include-ignored`. Without
  it the three that need the adapter are skipped and only the radio-free tests
  run.

```bash
# The radio-free tests — no adapter, ordinary parallelism, ~6s
./cargo-isolated.sh test --no-default-features \
    --features bluetooth-ble,bluetooth-ble-client \
    --test server -- server::bluetooth_ble --test-threads=100

# The three adapter tests. --test-threads=1 because they contend for the
# machine's single radio, and --include-ignored because they are #[ignore]d.
./cargo-isolated.sh test --no-default-features \
    --features bluetooth-ble,bluetooth-ble-client \
    --test server -- server::bluetooth_ble --include-ignored --test-threads=1

# One adapter test with full output
./cargo-isolated.sh test --no-default-features \
    --features bluetooth-ble,bluetooth-ble-client \
    --test server -- test_bluetooth_heart_rate_server --include-ignored \
    --test-threads=1 --nocapture
```

**A cold-start flake worth recognising before you debug it.** The four tests
that drive `run_event_loop_without_radio` (`read_default_value_test.rs`,
`llm_failure_test.rs`) can fail on the *first* run after a fresh link with
`Expected at least 1 calls, got 0` — the mock model is never reached at all,
and the read still answers because the fail-closed path handles the error. The
second run passes in a fraction of the time. This is the first-LLM-call stall
the root `CLAUDE.md` documents (building a `reqwest::Client` loads the macOS
keychain synchronously), not a defect in these tests. Re-run before
investigating; a genuine regression here fails on an *assertion*, naming the
ATT response it got.

**Expected output**:

```
=== E2E Test: Bluetooth Heart Rate Server ===
NOTE: This test requires a real Bluetooth adapter
Server started
✓ BLE adapter found
✓ Found NetGet-HeartRate device
✓ Connected to device
✓ Services discovered
✓ Found Heart Rate Service
✓ Found Heart Rate Measurement characteristic
✓ Read 2 bytes: [0, 72]
Heart rate BPM: 72
✓ Heart rate value verified: 72 BPM
✓ Disconnected from device
=== Test passed ===
```

## References

- ble-peripheral-rust: https://crates.io/crates/ble-peripheral-rust
- btleplug (client): https://crates.io/crates/btleplug
- Bluetooth SIG Services: https://www.bluetooth.com/specifications/assigned-numbers/
- BlueZ (Linux): http://www.bluez.org/
