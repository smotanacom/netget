# TFTP client test strategy

The TFTP client had **no tests at all** before August 2026, and could not have had any: it
was commented out of `client_registry.rs` and `src/client/mod.rs` in `66212c37`
(2025-11-20) because it did not compile against the then-new `call_llm_for_client` signature,
and nothing unregistered can be exercised.

## Peer honesty

The E2E test's server is a minimal RFC 1350 responder **written inside the test file**. That
is better than pointing the client at NetGet's own TFTP server — the two ends share no code,
so a symmetrical misreading of the RFC cannot cancel out — but it is still weaker than a
third-party implementation, and this project's history is full of protocols that passed
self-tests and were broken against real peers.

What is available here and why it is not used:

- `/usr/bin/tftp` (macOS) is a **client**. Useless for testing a client.
- `/usr/libexec/tftpd` (macOS) is a real, independent server, but it runs only under
  inetd/launchd — it expects the datagram on stdin — and cannot be driven from a test without
  a socket-activation shim.

If a standalone TFTP server (`tftp-hpa`, `atftpd`) becomes available on this machine, it
should replace the in-test responder. Until then, `metadata().e2e_testing` says plainly that
the peer is not independent.

## Layer 1 — codec against RFC 1350 literal bytes

LLM calls: **0**.

| Test | What it pins |
|---|---|
| `builds_rrq_and_wrq_exactly_as_rfc_1350_specifies` | `opcode \| filename \| 0 \| mode \| 0`, byte for byte |
| `decodes_data_ack_and_error_packets` | the three opcodes a client receives |
| `a_final_data_block_is_shorter_than_512_bytes` | the end-of-transfer rule |
| `rejects_truncated_and_client_only_opcodes` | short datagrams, and RRQ (which a client never receives) |
| `tolerates_an_unterminated_error_message` | a malformed peer must not take the client down |

## Layer 2 — E2E, LLM mocked

`reads_a_two_block_file`: **5 LLM calls** (startup, `tftp_connected`, two
`tftp_data_received`, `tftp_transfer_complete`).

Two things make it worth its cost:

1. **The mock ACKs dynamically.** `respond_with_actions_from_event` reads `block_number` off
   the event. A static `{"block_number": 1}` would stall the transfer at block 1 and the test
   would then be "fixed" by weakening its assertions — the failure mode the root `CLAUDE.md`
   warns about for UDP protocols.
2. **The server answers from a fresh TID.** RFC 1350 §4 requires the server's first reply to
   come from a newly allocated port, and subsequent client packets to go *there*, not to port
   69. A client that keeps replying to the well-known port produces a transfer that hangs
   after block 1. The in-test server binds a second socket precisely to exercise this, and
   `acked_blocks == [1, 2]` can only hold if the client followed it.

Assertions are on what the server observed — filename, mode, the ACK sequence — not on log
strings.

## Not covered

- **Write transfers.** `tftp_write_file` and `send_data_block` are implemented and the events
  are emitted, but there is no test. A write transfer costs one LLM call per 512-byte block,
  which makes a meaningful test expensive; this is the obvious next thing to add.
- **Retransmission**, because there is none: a 5-second timeout aborts (documented in
  `metadata()`).
- **RFC 2347 option negotiation** (`blksize`, `timeout`, `tsize`) — not implemented.
- `netascii` line-ending translation. The mode string is carried to the wire but the payload
  is passed through unmodified.

## Running

```bash
CARGO_TARGET_DIR=/tmp/tgt ./cargo-isolated.sh test --no-default-features --features tftp \
    --test client::tftp -- --test-threads=100
```
