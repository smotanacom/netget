# BLE Beacon (iBeacon / Eddystone)

**Maturity: Experimental. Linux only.** The payload construction is verified byte-for-byte
against the published layouts; the BlueZ transport has never been compiled or run on Linux.
Read the "What is verified" section before trusting it.

## What this protocol is, and what it is not

A beacon is **not** a GATT server. It accepts no connections, exposes no characteristics and
answers no reads. A beacon *is* its advertising payload: an iBeacon is a 23-octet blob of Apple
manufacturer-specific data, an Eddystone frame is service data published under 0xFEAA. Nothing
else about it exists.

That is why this protocol no longer wraps the `bluetooth-ble` base stack. The base's
`start_advertising` accepts a device name and a list of service UUIDs, which is precisely the
one shape a beacon cannot be built out of. It now owns its own transport
(`advertise.rs`) and its own LLM vocabulary (`actions.rs`).

## Platform support

| Platform | Can set an advertising payload? | Behaviour |
|---|---|---|
| Linux | Yes — `org.bluez.LEAdvertisement1` has `ManufacturerData` and `ServiceData` | Advertises |
| macOS | **No** | `spawn()` returns `Err` |
| Windows | Manufacturer data only, and unimplemented here | `spawn()` returns `Err` |

`-[CBPeripheralManager startAdvertising:]` documents exactly two honoured keys —
`CBAdvertisementDataLocalNameKey` and `CBAdvertisementDataServiceUUIDsKey` — and states that
every other key is ignored. This is a CoreBluetooth restriction, not a limitation of any Rust
wrapper, so writing bindings by hand would change nothing. `ble-peripheral-rust` 0.2 exposes no
advertising-payload API on any platform.

The refusal is deliberate and is the point: this protocol previously started "successfully" on
macOS and sat in `Running` while broadcasting something no beacon scanner could recognise.
`server_startup.rs` turns the `Err` into `ServerStatus::Error` with the reason.
`advertise::UNSUPPORTED_PLATFORM_MESSAGE` is the single source of that text.

## Library choice: `bluer`, not a second D-Bus stack

`bluer` 0.17 was already in the dependency graph — `ble-peripheral-rust` uses it for its own
Linux backend — so it is declared directly rather than adding another D-Bus crate. It speaks
exactly the API needed: `Adapter::advertise(Advertisement { manufacturer_data, service_data,
service_uuids, local_name, .. })` publishes an `org.bluez.LEAdvertisement1` object and registers
it with `org.bluez.LEAdvertisingManager1`. `btleplug` cannot do this — it is a central-role
library and has no peripheral or advertising API at all.

The dependency is target-gated in `Cargo.toml`:

```toml
[target.'cfg(target_os = "linux")'.dependencies]
bluer = { version = "0.17", optional = true, features = ["bluetoothd"] }
```

Enabling `bluetooth-ble-beacon` on macOS or Windows is therefore a no-op for the dependency; the
feature still compiles, and the protocol still registers, so the model gets a real error rather
than a missing protocol.

## Files

| File | Contents |
|---|---|
| `payload.rs` | Pure byte construction. No I/O, no platform `cfg`, no Bluetooth. |
| `advertise.rs` | `BeaconAdvertiser`: the BlueZ path under `cfg(target_os = "linux")`, the refusal otherwise. |
| `actions.rs` | `ProtocolActions`: the four actions, the one event, `execute_action_with_state`. |
| `mod.rs` | `spawn`, and `BeaconServer` — the live-instance handle. |

## Actions

All parameters are structured. **No action carries payload bytes**, because the whole value of
this protocol is that it builds the octets so the model does not have to.

| Action | Parameters |
|---|---|
| `start_ibeacon` | `uuid` (128-bit), `major` (0-65535), `minor` (0-65535), `measured_power` (dBm at 1 m, default -59) |
| `start_eddystone_uid` | `namespace` (20 hex digits or a UUID), `instance` (12 hex digits), `tx_power` (dBm at 0 m, default -20) |
| `start_eddystone_url` | `url` (http/https), `tx_power` (default -20) |
| `stop_beacon` | none |

Each `start_*` replaces whatever is on air — BlueZ would otherwise rotate between two registered
advertisements.

`namespace` and `instance` are hex strings, and that is not a violation of the no-bytes rule for
the same reason a UUID is written in hex: they are identifiers in their published textual form,
they have a fixed width, and `BeaconFrame::eddystone_uid` really decodes them. The documented
encoding and the executor agree.

## Events

| Event | Emitted | Offered actions |
|---|---|---|
| `beacon_started` | Once, from `spawn`, after the adapter opens | all four |

**One event, and it really fires.** A legacy beacon advertisement is one-way, so nothing ever
arrives and there is nothing else to report. Declaring `beacon_stopped` or `beacon_updated`
would produce the failure documented in the root `CLAUDE.md`: an event that is advertised to the
model, that an `event_handlers` pattern can be keyed on, and that can never fire.

## How an action reaches the radio

The protocol object in the registry is zero-sized and holds no adapter, so `execute_action`
cannot transmit. `spawn` registers a `BeaconServer` via `AppState::register_server_handle()`,
and `BluetoothBleBeaconProtocol::execute_action_with_state` looks it up by `server_id` —
the mechanism described on `Server::execute_action_with_state`, which until now no protocol
used.

`execute_action` (the stateless variant) validates and then **fails closed** with a message
saying the live handle is required. It is unreachable today, since `executor::execute_actions`
always calls the state-aware variant; failing rather than returning a success that transmitted
nothing means a change in that plumbing would be loud.

`AppStateInner::teardown_server` drops the handle, which drops `bluer`'s `AdvertisementHandle`,
which
unregisters the advertisement from `bluetoothd`. There is no accept loop and no event loop, so
nothing is registered with `register_server_task()` — there is no task to cancel.

## No fallback beacon

If the model (or a script/static handler) answers `beacon_started` with no `start_*` action,
**nothing is broadcast** and a WARN says so on both the log and the status stream. Inventing a
default UUID would put an unattributable beacon on the air that nobody asked for, which is the
fail-open pattern the root `CLAUDE.md` warns about.

## The 31-octet budget

Legacy advertising carries 31 octets. `payload.rs` accounts for every one of them:

| Frame | AD octets used | Room for a device name |
|---|---|---|
| iBeacon | 30 | none (1 spare, a name costs 2 before any characters) |
| Eddystone-UID | 31 exactly | none |
| Eddystone-URL `https://example.com/` | 22 | 7 characters |

`device_name` is therefore advertised only when it fits, truncated on a char boundary
(`utils::truncate::truncate_str` — a byte-index cut through multi-byte UTF-8 panics, and this
name comes from LLM or MCP input) and tagged as a *shortened* local name (0x08) rather than a
complete one (0x09) when anything was removed. An iBeacon never carries a name, which is correct:
an iBeacon advertisement is manufacturer data and nothing else.

Eddystone-URL compresses with both published tables — the four scheme prefixes and the fourteen
`.com/`-style substitutions — and refuses a URL that exceeds 17 encoded octets or contains a
character outside printable ASCII, rather than truncating it into a different URL.

## Startup parameters

- `device_name` (string, optional, default `NetGet-Beacon`) — advertised only if the frame leaves room
- `adapter` (string, optional) — e.g. `hci0`; defaults to the system default adapter

Both are declared in `get_startup_parameters()` and both are read. Anything else — including
`uuid`, which belongs to an *action* — is refused by name at startup.

## Privilege

`privilege_requirement` is `None`. BLE advertising needs adapter access, not a port, raw sockets
or root, and `PrivilegeRequirement` has no variant for device access (root `CLAUDE.md`,
IMPROVEMENTS item 60). Claiming `Root` would refuse to start for users who can in fact use the
adapter. In practice a Linux user needs D-Bus permission to talk to `org.bluez` — usually
membership of a `bluetooth` group, or a polkit rule.

## What is verified, and what is not

**Verified** (on macOS/arm64, `tests/server/bluetooth_ble_beacon/payload_test.rs`, 24 test
functions on a non-Linux host — 23 `#[test]` plus `non_linux_refuses_to_start`, which is a
`#[tokio::test]` compiled only under `cfg(not(target_os = "linux"))`, so the count is 23 on
Linux). Derive it rather than trusting it; this line said 22 and was never right:

- iBeacon manufacturer data and full AD, against Apple's published layout, with literal
  expected bytes including big-endian major/minor and the signed measured-power octet.
- Eddystone-UID service data and full AD against `google/eddystone`, including the 0x17 service
  data length and the two RFU octets.
- Eddystone-URL: all four scheme codes, all fourteen suffix codes, the full AD, the 17-octet
  boundary asserted from both sides, and refusal of a bad scheme, a space and a non-ASCII
  character.
- The 31-octet budget for every frame, name fitting and char-safe truncation.
- Structured action parsing, the defaults, and rejection of malformed input.
- `beacon_started` offers all four actions (the family's reachability trap).
- On non-Linux, `BeaconAdvertiser::open` and `Server::spawn` both return the platform error and
  leave no server handle behind.

**Not verified. Nothing has ever been transmitted.**

- The `bluer` code in `advertise.rs` has not been compiled on Linux. It was written against the
  `bluer` 0.17.4 sources (`src/adv.rs`, `src/adapter.rs`, `src/session.rs`) rather than from
  memory, and `uuid` resolves to a single version tree-wide so `bluer::adv::Advertisement`'s
  `Uuid` is ours — but a cross-check was not possible here (the host toolchain has no Linux
  `std`, and `libdbus-sys` would need a Linux sysroot regardless).
- No BLE scanner has confirmed a frame on air, so nothing proves BlueZ composes the AD the way
  the layouts in `payload.rs` describe. In particular BlueZ, not this code, decides the flags
  octet and whether `Type::Broadcast` includes one at all.
- `Session`/`Adapter`/`AdvertisementHandle` are assumed `Send + Sync + 'static` so that
  `Arc<BeaconServer>` satisfies `register_server_handle`. That will be checked by the first
  Linux compile.

Treat the first run on Linux as bring-up. `tests/server/bluetooth_ble_beacon/e2e_test.rs` has an
`#[ignore]`d Linux test to start from.
