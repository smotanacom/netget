# CAN bus tests — strategy

**43 tests, all passing, no `#[ignore]`s.** 28 in `frame_test.rs`, 15 in `e2e_test.rs`.
**LLM budget: 2 calls**, both in `e2e_test.rs`.

## What can and cannot be tested here

`AF_CAN` is a Linux kernel address family. This suite runs on macOS, where it does not exist at
all — no socket family to bind, no `can0`, no `cansend`. **Nothing in this directory says anything
about SocketCAN**, and no assertion here should ever be read as evidence that a frame reached a
real CAN bus.

What *is* tested is everything else, and it is deliberately most of the protocol:

| Layer | Covered by | How |
|---|---|---|
| Frame representation, validation, DLC tables, struct layout, error decoding, action parsing | `frame_test.rs` | Literal values |
| Registry entry, metadata, action/event declarations | `e2e_test.rs` | Direct registry queries |
| The platform refusal | `e2e_test.rs` | `spawn()` must return the exact `Err` |
| Event → handler/script/LLM → action → frame, both directions | `e2e_test.rs` | In-process over the UDP test transport |
| The `AF_CAN` transport | **nothing** | See `src/server/can/CLAUDE.md` for the Linux experiment that would |

## `frame_test.rs` — literals, and only literals

Every expectation is a byte array or a number written out by hand. **None is computed by calling
the code under test with different arguments**, because that proves only that a function agrees
with itself. `rss` sat at Experimental for months on exactly that mistake — the `rss` crate
parsing what the `rss` crate had built — and the correction is recorded in the root `CLAUDE.md`.

The expectations worth knowing about:

- **`fd_dlc_is_a_length_code_not_a_byte_count`** writes out all sixteen CAN FD length codes in
  both directions. This is the single most common CAN FD implementation error: a stack that
  assumes `len == dlc` puts a frame of the wrong size on the wire and nothing fails loudly.
- **`an_unencodable_fd_length_is_refused_rather_than_padded`** — CAN FD has no encoding for 9, 10,
  11, 13… bytes. Refusing is the requirement; padding up would append bytes the model did not
  write and truncating down would drop bytes it did.
- **`a_standard_data_frame_encodes_to_the_can_frame_struct`** and its FD twin pin `struct
  can_frame` (16 octets) and `struct canfd_frame` (72) octet by octet, including the little-endian
  identifier word, the `flags` byte that sits where classic has padding, and the fact that the
  struct's `len` field is a byte **count** while the DLC reported to the model is the wire code.
- **`standard_and_extended_zero_x_123_are_different_frames`** — `extended` is a field, never
  inferred from magnitude, and the two encodings must differ.
- **`text_and_hex_are_different_encodings_of_the_same_string`** — the `send_tcp_data` defect the
  root `CLAUDE.md` records, asserted from both sides. `"48656c"` as hex and as text produce
  different bytes, and the executor reads the declared `encoding` rather than sniffing.
- **`the_bus_state_ladder_decodes_from_the_controller_status_octet`** builds the kernel's error
  frames by hand (`CAN_ERR_CRTL` in the identifier, the status bits in `data[1]`) and asserts each
  rung of error_active → error_warning → error_passive → bus_off, plus that an RX overflow is
  *not* a confinement state.

## `e2e_test.rs` — in-process, over the declared UDP test transport

The suite builds a real `SpawnContext` and calls `Server::spawn` directly, the way
`tests/server/lldp/e2e_test.rs` and `tests/server/bluetooth_ble_beacon/e2e_test.rs` do. It asks
the protocol for its declared `transport: "udp"`, which carries the same SocketCAN frame structs
in datagrams — so the real codec, the real event, the real dispatcher and the real action executor
all run.

### The refusal is a test, not a gap

`the_socketcan_transport_refuses_to_start_off_linux` is the most important test in the file. It
asserts that with the default transport, `spawn()` returns `Err`; that the message names `AF_CAN`,
Linux, the `vcan0` route and the UDP alternative; and that it is **byte-identical to
`transport::UNSUPPORTED_PLATFORM_MESSAGE`**, so the documentation and the runtime error cannot
drift. It then starts a UDP-transport server on the same host, which is what makes the refusal a
routing decision rather than a dead end.

It **skips on Linux**, printing why. Inverting it there would have this suite assert something
about SocketCAN that it is not entitled to claim.

### LLM budget: 2

| Test | Calls | Mock |
|---|---|---|
| `the_model_is_the_ecu` | 1 | `can_frame_received` → an OBD-II response on 0x7E8 |
| `a_can_fd_frame_survives_the_whole_path` | 1 | `can_frame_received` → a 16-byte FD frame with BRS on a 29-bit identifier |

Both finish with `wait_for_expectations(30)` then `verify_calls()` — without the second, the test
asserts nothing about LLM interaction at all.

**Every other test points at `http://127.0.0.1:1`**, an endpoint nothing listens on. That is
stronger than counting calls: a frame arriving proves no model call was *needed*, rather than
merely that one was not counted. It is the trick `bluetooth_ble_beacon` and `lldp` use.

### Silence, four ways

The protocol's whole failure design is that an LLM failure transmits nothing, because CAN has no
error reply — a CAN error frame corrupts a frame in flight and repeated ones drive nodes bus-off.
On the wire every outcome is identical, so each test asserts the `decision=` tag *and* that no
datagram followed:

| Test | Asserts |
|---|---|
| `an_llm_failure_transmits_nothing` | `decision=fail_closed_*`, "nothing transmitted", and no frame |
| `no_response_is_a_decision_and_is_tagged_as_one` | `decision=model_reject`, and no frame |
| `with_no_policy_the_server_listens_and_never_calls_the_model` | `decision=no_policy`, and no frame |
| `no_action_can_emit_an_error_frame` | no action exposes an `error` parameter, and even an action carrying `"error": true` produces a data frame |

The "no frame" check is always made **after** the decision is known, so it is not racing a frame
still in flight.

### The two events that are easy to declare and never emit

`an_error_frame_raises_can_error_frame` and
`crossing_a_confinement_boundary_raises_can_bus_state_changed_once` each attach a static handler
to one event only, so a frame coming back is proof that specific event was dispatched. The second
also sends the same error frame twice and asserts **nothing** comes back the second time: a
degrading bus repeats its error frame, and raising the event on a non-transition would flood the
model with reports that say nothing new.

`tests/event_emit_sites_test.rs` guards the declaration side across the whole tree; these two
guard the behaviour.

## Waiting, not sleeping

`Running::wait_for_status` polls the status stream against a deadline. A fixed sleep long enough
to be reliable under `--test-threads=100` would make every test in the file slow, and the repo has
been bitten by exactly that — whole suites used to `sleep(1)` and then verify, which is enough
alone and not when a hundred run together.

The one `sleep` in the file is in
`crossing_a_confinement_boundary_raises_can_bus_state_changed_once`, where the assertion is that
**nothing** arrives; there is no condition to wait for, only an interval to give a frame the
chance to appear.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features can --test server can:: \
    -- --test-threads=100
```

Two whole-tree ratchets are worth running alongside, since this protocol is exactly the shape they
exist to catch:

```bash
./cargo-isolated.sh test --no-default-features --features can \
    --test event_emit_sites_test --test startup_param_drift_test \
    --test executable_examples_test --test wire_failure_test -- --test-threads=100
```

Two failures in that second command are **pre-existing and unrelated** — they assert whole-registry
coverage (`checked > 900` examples; at least one client registered) that no single-feature build
can satisfy. They fail identically with any other lone protocol feature. The tests that matter
here — `every_server_event_type_has_an_emit_site`,
`no_new_declared_startup_parameter_goes_unread`,
`no_action_ships_an_example_its_own_executor_refuses` and
`no_server_protocol_interpolates_an_error_into_a_peer_visible_string` — all pass.

## What would make this suite mean more

Nothing that can be done on macOS. The next step is the Linux `vcan` experiment in
`src/server/can/CLAUDE.md`: `cansend` and `candump` from `can-utils` are real third-party peers
that share no code with the `socketcan` crate, so they satisfy the independence rule that
`websocket` and `webrtc_signaling` fail. Until someone runs it, the SocketCAN transport is
unverified and the rating stays `Experimental`.
