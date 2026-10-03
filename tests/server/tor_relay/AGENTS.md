# Tor Relay E2E Tests

`tests/server/tor_relay/e2e_test.rs` (one test, **2 LLM calls**, not `#[ignore]`d) and
`tests/server/tor_relay/llm_failure_test.rs` (two tests; each has its startup call mocked and
one event deliberately left unmocked so the mock answers HTTP 500). Both drive the relay with the Tor client in `peer.rs`.

## Strategy

The peer is a Tor client written from tor-spec, in `peer.rs` in this directory — the ntor
handshake (5.1.4), its 92-byte KDF layout (5.2.2) and AES-128-CTR relay-cell crypto (6.1). It
does not call into the server's `circuit.rs`, and that independence is the whole point: if the relay's
key derivation, its forward/backward key assignment, or its cell layout is wrong by a byte, the
client derives different keys, the AUTH check fails, and the test fails.

Arti is deliberately not used: it is a full Tor client, it needs a signed consensus to
bootstrap, and it would hide exactly the implementation details being tested.

## What it proves

1. **VERSIONS framing.** An 11-byte VERSIONS cell (2-byte circuit id, variable length) is
   framed and answered with link version 4. The reader used to `read_exact` 514 bytes and block
   on it forever — a real `tor` logged `died in state handshaking`.
2. **A genuine ntor handshake.** The client recomputes the server's AUTH value from
   `secret_input` and checks it, then derives Kf/Kb. This needs the relay's identity
   fingerprint and onion public key, which the test reads out of the server's startup log — a
   real relay publishes both in its descriptor, and this one logged only the fingerprint until
   this suite existed, which meant no peer could complete a circuit at all.
3. **A real exit stream.** RELAY/BEGIN to a localhost HTTP server is answered with
   RELAY/CONNECTED, and RELAY/DATA carries an HTTP request out and the response back. The
   response is decrypted with Kb and the body asserted, so the keystreams stay in step across
   cells and the directions are the right way round.

`recv_relay()` also asserts the `recognized` field decrypts to 0 and that the declared length
fits in a cell — both fail loudly if the backward keystream has drifted, which is otherwise a
silent corruption.

## What `llm_failure_test.rs` proves

The startup instruction and `tor_relay_circuit_created` are mocked, so the circuit comes up
normally; `tor_relay_relay_cell` is deliberately left unmocked, so the mock answers HTTP 500
and `call_llm` returns `Err`. A RELAY/EXTEND — a command this relay does not implement, so the
model is the whole answer — must then be answered with a **DESTROY cell, reason 2 INTERNAL**,
514 bytes with the rest of the payload zero. It asserts the reply is *not* a RELAY cell (the
peer would read that as the EXTENDED it asked for) and that the DESTROY carries the right
reason.

`test_tor_relay_refuses_circuit_when_llm_fails_on_create2` leaves `tor_relay_circuit_created`
unmocked instead, so the model fails on the CREATE2 itself. The reply must be DESTROY / reason
2 INTERNAL, not CREATED2 — CREATED2 is admission, and admitting a circuit the model could not
rule on is fail-open — and the log must carry `decision=fail_closed_llm_error`. It was checked
by restoring the fall-through to CREATED2, at which point it fails on the `assert_ne!` against
CREATED2. `RelayPeer::send_create2_expecting_any` exists for it: `create_circuit` asserts the
reply is a CREATED2 and so cannot observe the alternative.

No exit stream is opened and nothing outside 127.0.0.1 is contacted.

## What it does not prove

No real `tor` or Arti binary is involved and cannot be: the link handshake stops after VERSIONS
(no CERTS / AUTH_CHALLENGE / NETINFO). Relay cell digests are never computed or verified, there
is no EXTEND, and no exit policy is enforced. That is why the protocol stays `Experimental`;
the list lives in `src/server/tor_relay/CLAUDE.md`.

## LLM call budget

1 for server startup, 1 for the `tor_relay_circuit_created` event, in `e2e_test.rs` and in the
RELAY-failure test. The data path (BEGIN, DATA, END, SENDME) is decided in Rust and raises no
events. The RELAY-failure test additionally provokes one `tor_relay_relay_cell` call that has no
rule and is therefore answered 500; the CREATE2-failure test has only its startup call mocked
and provokes one unmocked `tor_relay_circuit_created`. Those failures are the thing under test,
so they carry no expectation.

**The circuit-created mock must answer with an action that produces no output.**
`detect_relay_cell` is the right one: an `Output` action from that event *replaces* the CREATED2
cell the relay was about to send, and the circuit never completes.

## Trap that hid every bug above

The previous version of this file passed `base_stack: "TorRelay"`. The registry name is
`"Tor Relay"`, with the space, and `open_server` rejects the other — so the test could never
have started a server. It did not matter, because it was `#[ignore]`d and printed `✓` for every
outcome including a timeout. Do not reintroduce either.

## Manual testing with a real Tor client

Still not possible. Once CERTS/AUTH_CHALLENGE/NETINFO exist, the shape is:

```bash
./cargo-isolated.sh run --features tor --release
# Prompt: "Start a Tor exit relay on port 9001"
curl --socks5 127.0.0.1:9050 http://example.com
```

## References

- [tor-spec](https://spec.torproject.org/tor-spec/) — sections 3 (cells), 4 (link handshake),
  5 (circuits, ntor), 6 (relay cells), 7 (flow control)
