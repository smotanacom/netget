# DataLink Client Tests

## Three files, split by what they need

| File | Privilege | What it covers |
|---|---|---|
| `action_test.rs` | **none** | the whole model-facing surface: every advertised action, every declared example, what `execute_action` accepts and refuses, and the event payload against literal frame bytes |
| `command_channel_test.rs` | none, plus one `#[ignore]`d privileged test | the lifecycle: `connect` refusing an interface it cannot open, and — privileged — a real acknowledged `sendpacket`, honest `[ send ]` outcomes, and the capture loop actually stopping |
| `e2e_test.rs` | **capture access, hard-required** | the real binary against a mock model, opening a real libpcap handle on loopback and injecting real frames |

## The correction this split makes

This directory used to claim, in this file, that the tests need "**No root privileges**", do
"**No actual network traffic**", and that "**Tests don't actually inject frames**". All three
were false. `e2e_test.rs` has always spawned the netget binary, opened `lo0` through libpcap and
put ARP frames on it; it passed on the maintainer's machine because that machine is in the
`access_bpf` group, and on any other host it failed as a mock expectation that never fired —
several steps from the cause.

That is fixed in the honest direction rather than the convenient one. `e2e_test.rs` now calls
`require_capture(...)` first and **fails, loudly, with the reason** when the access is missing.
It is deliberately not a skip: this client's only end-to-end evidence is those four tests, and a
silent pass would leave the maturity rating resting on nothing (the `npm` precedent).

What was genuinely missing was a test that needs no privilege *at all* — the client's advertised
actions, its frame validation and its event payloads were covered by nothing an ordinary run
executed. That is `action_test.rs`, and it is where any new assertion about what the model sees
should go.

## Tests

### `action_test.rs` (15 tests, no privilege, no LLM)

- every declared `example`, from async actions, sync actions and every event's action list, is
  accepted by this protocol's own `execute_action`
- all three events declare actions, and each is executable
- startup parameters are exactly `interface` and `promiscuous`
- `PrivilegeRequirement::PacketCapture`, agreeing with the live probe — **not** `RawSockets`,
  which is a different capability (a ChmodBPF user has one and not the other)
- `inject_frame` does not tell the model to append an FCS (it used to; the interface computes
  it, so a compliant model corrupted every frame with four bytes of payload)
- a 42-byte ARP frame decodes to exactly its bytes, header checked field by field
- `ff:ff:…`, `ff-ff-…` and space-separated hex all decode — a model writing from a packet dump
  writes separators, and they carry no information
- a runt (< 14 bytes) and an over-long frame (> 65535) are refused **by name**, before libpcap
- bad hex, an unknown verb and a missing `frame_hex` are refused, each naming what was wrong
- `disconnect` and `wait_for_more` map to their lifecycle results
- the event payload: a short frame reported whole; a long one cut with `truncated` and
  `captured_length` set and `frame_length` still the true length; the exact boundary uncut; and
  every field it emits declared on the events that carry it

### `command_channel_test.rs`

| Test | Privilege | Asserts |
|---|---|---|
| `datalink_client_refuses_an_interface_it_cannot_open` | none | `create` returns `Err` naming the device; the client is `ClientStatus::Error`; **no command handle** is registered |
| `connect_outcome_on_loopback_matches_capture_privilege` | none | `Ok` **iff** the process can capture; unprivileged, the refusal names `/dev/bpf*` or `CAP_NET_RAW` |
| `injected_frame_is_transmitted` | capture (`#[ignore]`d) | `Sent { bytes_sent }` from a real acknowledged `sendpacket`; bad hex and unknown verbs still `Rejected`; the injection reaches the access log; `disconnect` stops the capture loop |

The second test is the one with teeth on either kind of host: it fails if `connect` ever again
returns `Ok` without a capture handle, which is the bug this client shipped with.

The third one is worth understanding. Before the `StopSignal` fix it did not fail — it **hung
the whole test binary**, because the pcap loop had no exit path and `Runtime::drop` waits for
blocking tasks. If it ever hangs again, that is what came back.

### `e2e_test.rs` (4 tests, capture required, ≤3 LLM calls each)

1. `test_datalink_client_inject_frame_with_mocks` — startup, `datalink_connected` answered with
   an ARP frame, `datalink_frame_injected` observed
2. `test_datalink_client_promiscuous_capture_with_mocks` — promiscuous capture of loopback
   traffic the test generates itself (it used to wait on whatever happened to cross `lo0`)
3. `test_datalink_client_inject_and_respond_with_mocks` — both directions on one client
4. `test_datalink_client_disconnect_with_mocks` — inject, then disconnect

All four `wait_for_mocks(30)` then `verify_mocks()`. The "mock verification not possible in
subprocess tests" notes they used to carry were stale — the calls immediately below them do
exactly that.

## Running

```bash
# everything that runs without privilege
./cargo-isolated.sh test --no-default-features --features datalink \
    --test client -- client::datalink --test-threads=100

# the privileged injection test, on a host with BPF access
./cargo-isolated.sh test --no-default-features --features datalink \
    --test client -- client::datalink --ignored --test-threads=100
```

On a host **without** capture access the four `e2e_test.rs` tests fail with an explanation of
what they need. That is the intended behaviour; see above.

## Coverage gaps

- No test injects a frame that a third party then receives and validates. `sendpacket` returning
  `Ok` and the byte count coming back is what is asserted; nothing independent confirms the
  frame's contents on the wire. That is the gap between this client and a `Beta` rating.
- The follow-up depth cap (`MAX_INJECTION_FOLLOWUPS`) is not exercised: provoking it needs a
  model that answers every injection with another, and a live capture to inject on.
- Promiscuous-mode drop counting under load is not exercised.
