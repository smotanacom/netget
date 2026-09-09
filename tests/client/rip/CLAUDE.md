# RIP Client Test Strategy

## What runs, and what each thing is evidence of

Three files, and the distinction between them matters more than the count.

### `e2e_test.rs` — codec round-trips (not end-to-end, despite the name)

**test_rip_packet_encoding** / **test_rip_packet_decoding**

`RipMessage` encodes and `RipMessage` decodes, so these assert only that one implementation
agrees with itself. They are still worth keeping: the assertions are hand-written RFC 2453 §4
byte literals (command, version, the 20-byte entry layout, a 24-byte Request), so they pin the
wire format rather than merely round-tripping. They are **not** evidence that a real RIP router
would accept anything.

- **LLM calls**: 0
- **Runtime**: <100ms

### `llm_path_test.rs` — the client's LLM path, mocked

**the_model_drives_request_response_and_the_follow_up_is_carried_out**

A stub UDP router answers any Request with a two-route RIPv2 Response; a `MockOllamaServer`
answers `rip_connected` with `send_rip_request` and `rip_response_received` with `disconnect`.
The chain is the assertion: the request reaches the router (so the connect-event answer was
executed), and the client reaches `Disconnected` (so the response event was raised *and* its
answer was executed too). A client that asked the model and discarded the reply — the most
common client defect in this repo — passes step one and hangs on step two.

**an_unsupported_rip_version_is_refused**

`send_rip_request` accepts 1 and 2 and refuses everything else. The executor used to coerce any
non-1 value to RIPv2 and then log the number the caller asked for, so a `version: 7` request
went out as v2 and the log said v7.

- **LLM calls**: 2 (both mocked; no Ollama)
- **Runtime**: ~1s

### `command_channel_test.rs` — the dashboard's `[ send ]`

Injects `send_rip_request` / `wait_for_more` / an unknown verb / `disconnect` from outside the
client's loop and asserts the datagram really arrives at a stand-in router. Points the client
at an unreachable LLM URL on purpose, so it also verifies that `connect_with_llm_actions`
survives a failing connected-event call.

- **LLM calls**: 0 (the endpoint is unreachable by design)

## No Ollama anywhere

Every test above runs unattended. The previous suite put the *only* coverage of the client's
LLM path behind `#[ignore]` and a live Ollama on `localhost:11434` — so in practice nothing
exercised `connect_with_llm_actions`'s LLM branch at all, which is how two
`MutexGuard`-in-a-`match`-scrutinee holds survived in exactly that stretch of code. An
`#[ignore]`d test is not evidence; it has been replaced by `llm_path_test.rs`.

## What is still not covered

- **A real RIP implementation.** No third-party RIP client or daemon has been pointed at
  netget's RIP client or server. Everything here is netget talking to a stub written from the
  RFC in the same repository. This is why RIP stays Experimental.
- **RIPv1 on the wire.** `send_rip_request` accepts `version: 1` and the encoder emits it, but
  no test drives a v1 exchange end to end.
- **Authentication** (RFC 4822) — not implemented.
- **Bursty input.** `Processing`/`Accumulating` are unreachable in this client (see
  `src/client/rip/CLAUDE.md`), so there is nothing to test and the queue drain has never run.
- **Router timeout, malformed datagrams, an empty routing table, a 25-entry maximum response.**

## Running

```bash
# Everything, no Ollama needed
./cargo-isolated.sh test --no-default-features --features rip --test client -- rip --test-threads=100

# Just the LLM path
./cargo-isolated.sh test --no-default-features --features rip --test client -- rip::llm_path --test-threads=100
```
