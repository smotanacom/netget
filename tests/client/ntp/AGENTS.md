# NTP Client Testing

## Test Strategy

Black-box E2E tests spawn the NetGet binary with a mocked model and point its NTP client at a
**local responder**: a plain `tokio::net::UdpSocket` bound to `127.0.0.1:0` inside the test.
Nothing contacts a public time server.

The responder answers every 48-byte client-mode (mode 3) request with a minimal valid server
reply (RFC 5905 §7.3): LI 0, VN 4, mode 4, a stratum chosen by the test, precision -20, the
request's transmit timestamp echoed as the origin timestamp, and a fixed instant
(2024-01-01T00:00:00Z, NTP seconds 3 913 056 000) as the reference, receive and transmit
timestamps. It counts the requests it answered.

## LLM Call Budget

Every test makes **3** mocked calls, each `expect_calls(1)`:

1. The startup prompt, answered with `open_client` for the responder's address.
2. `ntp_connected`, answered with `query_time`.
3. `ntp_response_received`, matched on the responder's stratum **and** on
   `transmit_timestamp` = 1704067200 (the fixed instant in Unix seconds), answered with
   `analyze_response`. A reply that did not come from the responder, or that the client
   misparsed, leaves this rule uncalled and fails `verify_mocks`.

Event rules come before the startup rule; they cannot match the startup call, which carries no
event. Total: 9 calls across the suite.

## Test Cases (`e2e_test.rs`)

1. **`test_ntp_client_query_time_server`** — literal `127.0.0.1:<port>` target, stratum 2.
   Also asserts the responder answered exactly one request: one `query_time` is one datagram.
2. **`test_ntp_client_stratum_analysis`** — the same against a stratum-3 responder, so the
   stratum the model sees is the one on the wire.
3. **`test_ntp_client_resolves_hostname_target`** — `localhost:<port>` target. The client
   resolves a `host:port` and prefers its IPv4 address, so the query reaches the responder on
   127.0.0.1 even where `localhost` resolves to `::1` first (macOS does). Asserts exactly one
   request arrived.

## `command_channel_test.rs`

Model-free (the client's LLM points at an unreachable URL). Injects `query_time` through
`AppState::send_to_client`, asserts the 48-byte request (`0x1b`) reaches a local socket, and
answers it so the client's receive completes. See the file header.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features ntp --test client -- ntp:: --test-threads=100
```

## Requirements

- Feature flag `ntp`; no network access, no Ollama (mocked).
