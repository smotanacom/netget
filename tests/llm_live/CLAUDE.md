# Live-LLM Protocol Suite

Real-model tests: the real netget code, driven by a **real Ollama model**
(default `qwen3.8:27b-mlx`), graded on protocol-correct behavior. No mocks.
This is an *evaluation harness* for models and prompting, not a regression
suite — a failure can mean the model, the protocol's prompting surface, or the
code.

**Every test is hand-authored from that protocol's own event and action
vocabulary.** There is no generic "walk the registry and check the model
picked some declared action" sweep; that was tried and removed, because it
burned hours of GPU time to assert almost nothing. If you add a protocol, read
its `actions.rs` and its `tests/server/<p>/` suite first and encode what the
protocol actually requires.

## Two shapes

**1. Wire suites** — protocols whose transport runs on this machine.

- `*_setup_via_llm` (`LiveProtocolTest`): a plain-language prompt goes to the
  TUI LLM, which must call `open_server` with the right stack/port/instruction.
  Asserts the server started and the port is live, then stops. One model call;
  the only behavior under test is setup.
- one test per request type (`LiveRequestTest`): the server is started
  **deterministically** via `netget --server <protocol> --port N <instruction>`
  (no model call at setup), then a real client sends a request and the test
  asserts the bytes that come back. Never chain the two — a setup flake must
  not fail a request test.

**2. Event-level suites** — protocols needing hardware, root, or a peer
(Bluetooth LE, USB, ICMP/ARP/IGMP/DataLink, OSPF/RIP/BGP/IS-IS, WireGuard/
OpenVPN/IPsec, NFC). Their *transport* is out of reach here; the decision the
model makes is not. `EventCase` feeds the exact event a real exchange produces
— hand-written, not synthesized — through netget's real `PromptBuilder`
network-event path, and asserts the model chose the one correct action **and
that its parameters are right**.

Those parameter checks are the point. Examples in the tree:

| Protocol | What the case proves |
|---|---|
| ICMP | echo reply quotes the request's identifier, sequence and payload, with source/destination swapped |
| ARP | reply swaps roles: our MAC/IP become sender, requester becomes target |
| OSPF | our Hello lists the neighbour's Router ID (without it the adjacency never leaves Init) |
| BGP | accepted OPEN carries our ASN and a non-zero BGP identifier; a refusal is a NOTIFICATION with a valid error code, never silence |
| RIP | advertised route uses a reachable metric (16 = unreachable would withdraw it) |
| BLE heart rate | `respond_to_read` value decodes to `[flags, bpm]` with the instructed BPM |
| BLE battery | value decodes to a single 0-100 byte |
| BLE beacon | `start_ibeacon` carries the instructed proximity UUID + major/minor |
| USB FIDO2 | approval quotes the event's own `approval_id`; a denial is `deny_request`, not silence |
| USB smartcard / NFC | refusals carry a real ISO 7816 status word (`69 xx`), never `90 00` |
| WireGuard | authorization quotes the peer's public key and pins non-empty allowed IPs |
| TURN | response echoes the 24-hex transaction id **and** copies `relay_address` verbatim (NetGet bound that socket; any other value is refused with 508) |
| Torrent-DHT | KRPC reply echoes `t` byte-for-byte; node ids are 40 hex chars; a bad announce token is error **203**, not silence |
| Torrent-Peer | handshake echoes the peer's `info_hash` (or it disconnects); a `piece` echoes the request's `index`/`begin` |
| Bitcoin | `pong` carries the ping's nonce — bitcoind matches its outstanding ping on it |
| RTP | media is *described* (`content`/`tone_hz`/`digits`), never sampled; only `content: raw` may carry hex bytes |
| WebRTC | a media-only offer this server cannot serve is refused explicitly, not accepted |
| RDP | selects a protocol **the client offered**; an unacceptable offer is refused with `HYBRID_REQUIRED_BY_SERVER` |
| VNC | the framebuffer is redrawn from scratch, so a keystroke redraw must repaint the fixed chrome too |
| gRPC | response satisfies the event's declared schema, unwrapped; a missing record is `NOT_FOUND`, not a 200 |
| Snowflake | `rowset` row width equals `rowtype` column count (the driver indexes rows by position) |
| YARN | `state` and `finalStatus` are distinct enumerations; ids are `application_<ts>_<seq>` |
| SAML | RelayState comes back unchanged; the assertion posts to the SP's ACS; `/metadata` is an `EntityDescriptor`, not an assertion |
| DoH / DoT | answer with the **DNS** actions they delegate (not something HTTP-shaped), echoing `query_id` |

Events declared `with_no_actions()` (MongoDB disconnect, OAuth2 revoke,
WebRTC-signaling disconnect, USB smart-card detach) are graded too: the model
is offered *only* the common actions there, so the correct answer is an
observation, and reaching for a protocol action would be rejected as unknown.
Writing those cases is what surfaced `usb_smartcard_detached`'s
`response_example` naming `wait_for_more` — an action that protocol neither
declares nor executes.

## Running

```bash
# Wire suites for the protocols you have compiled
NETGET_USE_OLLAMA=1 ./cargo-isolated.sh test --no-default-features \
    --features tcp,udp,http,dns,redis --test llm_live -- --test-threads=100

# Event-level suites (hardware protocols) — no transport needed
NETGET_USE_OLLAMA=1 ./cargo-isolated.sh test --no-default-features \
    --features all-protocols --test llm_live -- --test-threads=100 \
    bluetooth_ble:: usb:: rawnet:: routing:: vpn:: nfc::

# One test
NETGET_USE_OLLAMA=1 ./cargo-isolated.sh test --no-default-features \
    --features arp --test llm_live arp_reply_swaps -- --nocapture
```

Model resolution: `NETGET_LLM_TEST_MODEL` > `OLLAMA_MODEL` >
`DEFAULT_LIVE_MODEL`. The framework fails fast with the list of pulled models
if the requested one is missing. Without `NETGET_USE_OLLAMA=1` every test
skips.

`--test-threads=100` is safe: the framework holds a suite-wide lock for each
test, so live tests run one at a time regardless of thread count. This is
necessary — netget children share one local Ollama, and N concurrent startups
would queue N setup calls behind it and blow the startup timeout (the first
full run failed 16/17 tests exactly that way). Budget: an event case is one
model call (~45s at 27B); a wire request test is setup-free plus one call per
request; a setup test is one to two calls.

## Framework

`tests/helpers/llm_live.rs`:
- `live_llm_enabled()` — the opt-in gate; **every test starts with it**.
- `LiveProtocolTest` — model-driven setup, for `*_setup_via_llm` only.
- `LiveRequestTest` — deterministic `--server` startup; `.server_params(json)`
  for protocols that need them (SOCKS5 needs `filter_mode: "ask_llm"` or it
  never consults the model at all).
- `LiveServer::{tcp_roundtrip, tcp_greeting_roundtrip, tcp_session,
  udp_roundtrip, http_request}` — wire clients. `tcp_session` is for dialogues
  spanning several exchanges (IMAP tag correlation, IRC's NICK+USER burst).
- `expect_llm_answered()` — asserts from the logs that a live per-event model
  call produced the answer.

`tests/helpers/llm_live_case.rs`: `EventCase` + `ParamCheck`
(`contains` / `equals` / `non_empty` / `custom`). `EventCase` fails loudly if
the protocol does not declare the event, or if the expected action is not one
the event advertises — so protocol drift breaks the test instead of silently
weakening it. Cases skip when their protocol is not compiled in.

## Adding a protocol

1. Read `src/server/<p>/actions.rs` (events, their declared parameters, each
   event's action list and `response_example`) and `tests/server/<p>/` for the
   real client dialogue.
2. Write `tests/llm_live/<p>.rs`: wire tests if the transport runs here,
   otherwise `EventCase`s with realistic payloads.
3. **Declare it in `tests/llm_live/mod.rs`** — an undeclared file is silently
   never compiled, and this directory is not covered by the `orphaned-tests`
   CI job. Wire suites get a `#[cfg(feature = …)]`; event-level suites do not
   need one.
4. Assert the protocol's own invariants (correlation ids, framing, status
   codes, encodings), not just "some text came back". If a protocol has no
   such invariant, say so in the module doc.
