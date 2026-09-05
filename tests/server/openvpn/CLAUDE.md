# OpenVPN E2E tests

## Strategy

Two suites, both of which must be able to fail for the reason they claim to test.

### `wire.rs` — an independently written codec

Byte layout decoded and encoded with explicit offsets, never calling
`netget::server::openvpn::packet`. Both suites build their requests and decode the server's replies through it.

This exists because a test that checks NetGet's parser against NetGet's serializer proves nothing — both can be wrong in
the same way, and that is exactly how this protocol shipped a reset reply with the message packet id written before the
ACK array, which no real client could parse.

It also holds the captured frames. Every literal was taken off the wire from **OpenVPN 2.7.4**
(`aarch64-apple-darwin`, OpenSSL 3.6.2) against a reference responder written outside this repository:

| Constant                      | What it is                                                        |
|-------------------------------|-------------------------------------------------------------------|
| `CAPTURED_CLIENT_RESET_V2`    | The 14-byte `P_CONTROL_HARD_RESET_CLIENT_V2` a real client sends   |
| `CAPTURED_SERVER_RESET_V2`    | The 26-byte reply that client **accepted**                        |
| `CAPTURED_SERVER_ACK_V1`      | The 22-byte `P_ACK_V1` that client **accepted**                   |
| `CAPTURED_CLIENT_CONTROL_V1`  | Head of the `P_CONTROL_V1` carrying its TLS ClientHello           |

### `codec_test.rs` — wire format, reliability layer and key method 2, 0 LLM calls

Three groups:

1. **Wire format.** Parses the captured client frames and asserts the decoded fields; emits our reset reply and ACK and
   asserts they are **byte-identical** to the frames the real client accepted; round-trips; and feeds hostile input —
   empty, truncated, `ACK length 255` with nothing behind it, unknown opcodes, wrong-category opcodes, tls-crypt-v2 —
   asserting `Err` and no panic. `no_byte_string_can_panic_either_parser` fuzzes both parsers with 20,000 pseudorandom
   strings, half of them starting from a plausible opcode byte.

2. **Reliability layer.** `ReliableSender` / `ReliableReceiver` are driven directly, because none of the properties that
   matter can be provoked over a loopback socket that never loses anything — and every one of them is silently fatal to
   a TLS handshake if wrong. In-order delivery with buffering of what arrives early; a duplicate acknowledged again but
   never delivered twice; a packet outside the window neither acknowledged nor buffered; retransmission after the delay
   with an identical payload and packet id; nothing sent again once acknowledged; the session declared dead after the
   attempt budget; the send window holding back everything past four packets; and fragmentation that reassembles to the
   original flight.

3. **Key method 2.** The expected byte layout is written **by hand in the test file** from `ssl.c`'s
   `key_method_2_write`, not produced by the code under test. Covers: parsing a client message including `IV_*` peer
   info; a prefix reported as *incomplete* rather than invalid, at eight different cut points (the control channel is a
   byte stream and a message can span two `P_CONTROL_V1` packets); a wrong leading `u32` or key method rejected; and the
   server's answer matching the layout a client reads — **no pre-master secret**, and three genuinely *empty* strings
   (`u16` 0, no bytes) for username, password and peer info.

### `e2e_test.rs` — 6 tests, 6 LLM calls

Two of them drive the system's real `openvpn` binary. Both read the control-channel certificate fingerprint out of the
server's own startup log and pass it to `--peer-fingerprint`, which is OpenVPN 2.6+'s documented way to trust a
self-signed peer. No CA, no PKI, no root.

1. **`test_real_openvpn_client_completes_tls_and_key_exchange`** — the client must log, in order:
   - `TLS: Initial packet from [AF_INET]127.0.0.1:<port>` — it parsed our `P_CONTROL_HARD_RESET_SERVER_V2`.
   - `VERIFY OK: depth=0` — our certificate flight arrived intact over the reliability layer and matched the
     fingerprint. This is printed from OpenSSL's verification callback, i.e. **during** the handshake.
   - `Control Channel: TLSv1.x, cipher …` — printed only once the session is *established*, which on the client happens
     inside `key_method_2_read`. It is therefore evidence about the key exchange, not about the handshake; the two
     markers are deliberately not interchangeable.
   - `Peer Connection Initiated` and `SENT CONTROL […]: 'PUSH_REQUEST'` — it accepted our key-method-2 answer and went
     on to ask for its configuration.

   And it must **not** log `Initialization Sequence Completed`: the server answers no `PUSH_REQUEST`, so there is no
   tunnel. If that ever changes, the metadata and docs are wrong and this test says so.

   Server-side, the test asserts the username the client offered appears in the log — the credential capture is the
   reason this protocol is worth running, and it is only possible because the TLS session is real.

2. **`test_real_openvpn_client_is_refused_at_the_key_exchange`** — same client, same server, one handler changed to
   `reject_key_exchange`. The TLS handshake must still complete (`VERIFY OK: depth=0`) and `Peer Connection Initiated`
   must **not** appear. This pair is what makes the first test non-vacuous: the two runs differ only in the decision, so
   the marker cannot be passing for some unrelated reason.

3. **`test_reset_reply_is_spec_correct_and_control_packets_are_acked`** — raw UDP. Asserts the reply is 26 bytes,
   opcode 8, acknowledges the packet id actually sent, echoes the session id actually sent (not a constant), numbers its
   own packet 0, and carries no trailing bytes. Then: **an unacknowledged reply is retransmitted on its own**, byte for
   byte, without the client asking; a retransmitted reset gets the identical answer; a `P_CONTROL_V1` gets a 22-byte
   `P_ACK_V1` with no message packet id; and after being fed five hostile datagrams the server still answers a
   brand-new peer correctly.

4. **`test_garbage_tls_record_kills_only_that_session`** — a well-formed TLS record header wrapping nonsense must still
   be acknowledged (the reliability layer is beneath TLS and answers first), then fail the TLS session and be logged.

5. **`test_rejected_peer_receives_nothing`** — `reject_peer` is enforced: no bytes at all, including for a
   retransmission.

6. **`test_absent_decision_fails_closed`** — the handler runs and logs but produces neither decision. The peer must
   still receive nothing, and the log must distinguish "no decision" from an explicit refusal. This is the regression
   test for the fail-open pattern that OAuth2 shipped.

### `recv_opcode`, not `recv`

The reliability layer retransmits, so a specific reply is **not** necessarily the next datagram on the socket. The raw-
UDP tests filter by opcode rather than asserting on whatever arrives first; a test that assumed otherwise would fail for
a reason that has nothing to do with what it is checking.

## Privileges

**No test requires root.** The server has no TUN device, so there is nothing to elevate for. `--dev null` on the client
means it needs none either.

## `openvpn` must be installed

The real-client tests **fail** rather than skip when the binary is missing. A capability check that returns success when
the capability is absent is worse than no test: it reports coverage that does not exist. Install with
`brew install openvpn` or `apt-get install openvpn`.

## LLM call budget

6 calls total — one server startup per E2E test. Both peer decisions use static `event_handlers`, so no model call
happens per peer or per key exchange. The codec suite makes none.

## Running

```bash
./cargo-isolated.sh test --all-features --test server -- --test-threads=100 openvpn
```

Expected: `28 passed; 0 failed`, about 13 seconds. (`--no-default-features --features openvpn` works too and builds
much faster.)

The first run after a source edit rebuilds and can time out; run twice and use the second result.

## Verifying these tests can fail

Checked against two deliberate regressions:

- **Wire format**: `ControlFrame::serialize` reverted to the old field order (message packet id before the ACK array).
  All four original E2E tests and the three frame-emitting codec tests failed, including the real-client test.
- **Key method 2**: a 48-byte pre-master secret added to the *server's* answer, which is a client-only field.
  `server_key_method_2_matches_the_layout_a_client_reads` failed, and so did the real-client test — at
  `Control Channel: TLSv1`, confirming the client prints that line only after `key_method_2_read` succeeds.

The reliability layer was checked positively rather than by regression: with `MAX_CONTROL_PAYLOAD` temporarily reduced
from 1100 to **200**, the server's TLS flight splits into more packets than the 4-packet send window, so the run
exercises windowed release, ACK-driven advance and multi-fragment reassembly at both ends. The real-client test still
passed. Re-run that experiment if you change `reliable.rs` — a reliability test that cannot fail is the failure mode
this protocol already had once, in a different layer.

## References

- [OpenVPN protocol overview](https://openvpn.net/community-resources/openvpn-protocol/)
- [NetGet OpenVPN implementation](../../../src/server/openvpn/CLAUDE.md)
