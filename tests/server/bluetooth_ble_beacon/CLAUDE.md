# BLE Beacon Tests

## Strategy

A beacon is advertisement-only: no connection, no request, no response. There is nothing a test
can send it and nothing it will send back. Confirming a frame is really on air needs a **second
radio** running a scanner, on a Linux host with `bluetoothd` and an adapter. Neither CI nor a
macOS dev machine can do that.

So the suite is split by what is actually knowable:

| File | Runs everywhere | What it proves |
|---|---|---|
| `payload_test.rs` | yes | The advertising octets are correct, against literal spec-derived bytes |
| `e2e_test.rs` | yes | Registry wiring, startup parameters, and the non-Linux refusal |
| `llm_failure_test.rs` | yes | That a handler failure advertises **nothing**, says so, and keeps an overloaded backend distinguishable from a broken one |
| `e2e_test.rs` + `llm_failure_test.rs` (`#[ignore]`d Linux tests) | no | That BlueZ accepts the advertisement, and that a handler failure leaves nothing registered — run by hand on hardware |

**No LLM calls.** Nothing here starts the netget binary or a mock Ollama, so the budget is zero.
That is a deliberate change: the previous version of this directory spawned the binary three
times, mocked `bluetooth_ble_started`, and asserted a server started — which exercised the
`bluetooth-ble` base stack's LLM plumbing, needed a real adapter, and said nothing whatever
about beacons, at a time when the protocol could not emit a beacon frame at all.

## `payload_test.rs`

Every expected byte string is written out literally and derived from the published layout, not
from the implementation. Sources are named in the file header:

- Apple, "Getting Started with iBeacon" (2014) §2.1
- `github.com/google/eddystone`, `eddystone-uid` and `eddystone-url`
- Bluetooth Core Specification Supplement, Part A (AD structures, the 31-octet limit)

Coverage worth keeping if these tests are ever rewritten:

- **Endianness.** `ibeacon_major_and_minor_are_big_endian` uses 0x1234/0xABCD. Values like
  1 and 100 hide a byte swap; asymmetric ones do not.
- **Signed power octets.** -59 is 0xC5, -12 is 0xF4, -20 is 0xEC. A cast bug is invisible on 0.
- **AD length octets.** 0x1A for the iBeacon manufacturer structure, 0x17 for Eddystone-UID's
  service data, 0x0E for the example URL frame. The length counts the type and UUID octets and
  not itself; the pre-rewrite Eddystone-URL builder got exactly this wrong (it wrote `3 + len`
  where the answer is `6 + len`) and no test noticed because none existed.
- **The URL tables.** All four scheme prefixes *and* all fourteen suffix codes, each asserted
  separately. The old builder implemented the schemes and ignored the suffixes entirely, which
  costs four octets on `.com/` out of a 17-octet budget.
- **The 17-octet boundary from both sides**: 17 accepted, 18 refused.
- **Char-safe truncation** of the device name, with a multi-byte string whose cut point falls
  mid-character.

## `e2e_test.rs`

Constructs a real `SpawnContext` and calls `Server::spawn` in-process. The LLM endpoint points
at `127.0.0.1:1` on purpose: every assertion is about a failure that must happen *before* any
model call, so a test that could quietly reach a running Ollama would not be testing it.

The non-Linux test asserts three things, and the third is the one that matters: the error names
the platform, the error names the CoreBluetooth key that makes it impossible, and **no server
handle is left registered**. A refused spawn that leaves state behind is the half-started server
this whole change exists to prevent.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features bluetooth-ble-beacon \
    --test server bluetooth_ble_beacon -- --test-threads=100
```

On Linux hardware, add the ignored test:

```bash
cargo test --no-default-features --features bluetooth-ble-beacon \
    --test server bluetooth_ble_beacon -- --ignored --test-threads=100
sudo btmon    # in another terminal: confirm the ADV_NONCONN_IND payload
```

## What still has no coverage

Everything past `Adapter::advertise`. Nothing in this repository has ever put a beacon frame on
the air, and no test here can. Do not let a green run be read as "the beacon works" — it means
"the bytes are right and the platform gate is honest".
