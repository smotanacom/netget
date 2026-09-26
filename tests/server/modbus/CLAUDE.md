# Modbus TCP E2E Tests

## Strategy

Four layers, deliberately, because each one catches something the others cannot. The rating
these support is **Stable**; `src/server/modbus/CLAUDE.md`'s "Maturity: the six conditions"
says which file carries which condition.

1. **A real, independent client.** `tokio-modbus` 0.17 (MIT OR Apache-2.0) is a dev-dependency
   and drives the server in-process. It is a *different* implementation from
   `src/server/modbus/codec.rs`, which is hand-rolled — that asymmetry is what makes the test
   evidence rather than a tautology. If our byte counts, bit packing or exception framing were
   wrong, `tokio-modbus` would reject the response or decode it into the wrong values.

2. **Raw sockets with hand-built, spec-derived frames.** For behaviour that never reaches the
   model — unknown function codes, illegal quantities — and for MBAP fields that a client
   library hides. This is where the transaction-id and unit-id echo are asserted, and where two
   requests are sent in a **single TCP write** to prove the ADU framing loop really frames.

3. **Codec assertions against literal bytes.** `codec::encode_bits_response`,
   `encode_registers_response`, `encode_write_ack`, `parse_request` and `try_parse_adu` are
   checked against byte sequences taken from the Modbus specification's own examples
   (`0x01 0x02 0xCD 0x01` for nine coils, `0x03 0x04 0x02 0x2B 0x00 0x00` for two registers).

4. **An independent dissector.** `pcap_oracle_test.rs` hands a whole session — all eight
   function codes and an exception, requests and responses in order — to Wireshark's `mbtcp`
   through `tests/helpers/pcap_oracle.rs`. `e2e_test.rs::read_adu` does the same for the
   spec-rejected ADUs it reads.

**Nothing here asserts that a connection opened or that bytes arrived.** Every assertion is on a
decoded PDU: register values, coil bits, function codes, exception codes, MBAP fields.

## LLM call budget

| Test | Startup | Events | Total |
|---|---|---|---|
| `e2e_test::test_modbus_reads_writes_and_exceptions_against_tokio_modbus` | 1 | 8 | **9** |
| `e2e_test::test_modbus_spec_exceptions_and_mbap_framing` | 1 | 0 | **1** |
| `e2e_test::test_codec_*` (three tests), `test_out_of_range_*` | 0 | 0 | **0** |
| `real_client_test::test_modbus_reads_writes_and_exceptions_against_mbpoll` | 1 | 8 | **9** |
| `llm_failure_test::every_fail_closed_clause_answers_0x04_and_logs_which_one_it_was` | 1 | 9 matched + unmatched | **10** counted |
| `llm_failure_test::a_static_rule_answering_the_wrong_kind_fails_closed` | 0 | 0 | **0** |
| `bounds_test` (six tests), `connection_bounds_test` (two), `pcap_oracle_test`, `peer_inject_test` | 0 | 0 | **0** |

Every file is under the ~10-call target. The in-process files (`bounds_test`,
`connection_bounds_test`, `pcap_oracle_test`, `peer_inject_test`, and the second
`llm_failure_test`) build the server with `ServerForm` against a dead LLM endpoint and never
reach a model: they use requests the specification answers, `static` rules, or a `*` → `manual`
rule that parks a request for as long as a test needs.

In `llm_failure_test` the two wrong-kind answers each cost **two** calls: an action the event
does not offer is an unknown action to the LLM layer, which re-asks once before failing the
call. The write is deliberately answered by no rule, so the mock returns HTTP 500; those calls
are not counted against any expectation.

`peer_inject_test.rs` is the dashboard-injection test: a `*` static handler answers one FC 3
read over a raw socket (asserting both byte/packet counters), then `send_to_peer` proves an
injected `send_modbus_write_ack` is `Executed` without writing (request-bound Custom result)
and an injected `close_connection` yields `Disconnected`, EOF on the socket, and the peer
handle being released. It uses no mock LLM at all (`AppState` pointed at a dead port).

`test_modbus_spec_exceptions_and_mbap_framing` costs one call because starting a server costs
one; the exchanges it performs cost nothing, which is itself the assertion — those paths are
answered from the specification, not the model.

## Mock expectations

Seven rules in the first test. Two things to know about them:

- **Order matters.** `and_event_data_contains` is a substring match and rules are tried in
  order, so the *narrower* rule is declared first: the `register_type: input` rule precedes the
  `register_type: holding` one, and the `write_multiple_registers` rule precedes the
  `write_single_register` one. The coil-write rule matches `function` containing `coil`, which
  no register function name does.

- **Three rules use `respond_with_actions_from_event`.** Two derive their answer from the
  request's own `quantity` and `start_address` — the holding-register read returns
  `1800 + start + i*10` per register, the bit read returns a pattern of the requested width. A
  hardcoded array would pass just as well against a correct server, but would keep passing if
  the server started ignoring the requested quantity. The third accepts a coil write only if
  `coil_values` is exactly what the client sent, and refuses otherwise — so FC 15's bit
  unpacking is checked through the event rather than assumed. `real_client_test.rs` does the
  same for mbpoll's coil and FC 16 writes.

Every test ends with `server.verify_mocks().await?`. Without it the test asserts nothing about
LLM interaction at all.

## Why there is no `.respond_with_actions_from_event()` requirement for the ids

The CLAUDE.md rule about echoing transaction ids dynamically exists because DNS makes the model
supply `query_id`, so a static mock with a hardcoded id causes client timeouts. Modbus here does
**not** do that: `mod.rs` reconstructs the MBAP header from the request it parsed, and no action
parameter carries a transaction id. A static mock is therefore safe, and
`test_modbus_spec_exceptions_and_mbap_framing` asserts the echo directly against literal
transaction ids (`0xBEEF`, `0x0102`) that the mock never sees.

## Client library

`tokio-modbus` 0.17, `default-features = false, features = ["tcp"]` — no serial, no sync
wrapper, so the dependency is small (bytes, futures-core/util, tokio-util, log, thiserror,
byteorder, async-trait, crc, smallvec, most of which are already in the tree).

Its read/write methods return `Result<Result<T, ExceptionCode>, Error>`: the outer error is
transport, the inner is a decoded Modbus exception. Both layers are asserted — a refusal must
arrive as `Ok(Err(ExceptionCode::IllegalDataAddress))`, never as transport failure and never as
data.

## What is covered

- All eight function codes through **both** independent clients (`tokio-modbus`, `mbpoll`):
  FC 1/2 bits in order, ten across a byte boundary; FC 3 values; FC 4 refused with exception
  0x02; FC 5/6/15/16 accepted with each client validating the echo, and the written values
  checked on their way to the model
- FC 16 refused with exception 0x03 supplied by *name* (`"illegal_data_value"`), proving the
  name→code mapping in the executor (tokio-modbus)
- FC 0x08, unimplemented → exception 0x01, with no LLM call
- Quantity 0 and quantity 2001 → exception 0x03, with no LLM call; every quantity limit at its
  value and one past it (`bounds_test.rs::every_quantity_limit_is_exact`)
- MBAP transaction id and unit id echoed verbatim, protocol id 0, length field consistent
- Two ADUs in one TCP segment → two correctly framed responses; one ADU split across two
  segments → reassembled
- `unit_id` → exception 0x0B for another unit, pass-through for its own
- Every fail-closed clause in `src/server/modbus/CLAUDE.md`, on the wire and in the log
  (`llm_failure_test.rs`)
- Every declared bound from the wire, each verified by removal (`bounds_test.rs`,
  `connection_bounds_test.rs`): see the table at the top of `bounds_test.rs`
- `mbtcp` dissects a whole session of all eight function codes and an exception cleanly
- Codec: spec example frames, incomplete frames reported as incomplete (not as an error),
  non-zero protocol id reported as not-Modbus, bit packing, register packing, both write-echo
  shapes, exception encoding, malformed byte counts, out-of-range addresses

## Coverage gaps

- **No real PLC and no `pymodbus`.** Two independent client stacks drive every function code;
  a field device has never been pointed at this server.
- No concurrency test with the model in the loop: pipelined requests that are *not* both
  spec-rejected would each cost an LLM call. The framing loop is covered, and
  `bounds_test.rs::bytes_queued_behind_a_parked_request_are_capped` holds a request in
  `Processing` while bytes queue behind it, but the queued requests are never answered there.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features modbus \
    --test server -- server::modbus --test-threads=100
```

Runtime is about 40 seconds, and all of it is the two default-deadline tests in
`connection_bounds_test.rs`, which have to wait out the real 30-second first-byte bound. Every
other test finishes in a second or two. `mbpoll` (libmodbus) and `tshark` must be installed:
both tests that use them fail rather than skip without them.

## Failure modes seen so far

None. The suite has been stable across repeated runs. The most likely future flake is the
`wait_for_log("Modbus accept loop started", 15)` guard on a heavily loaded machine; the timeout
is generous precisely because the alternative — sleeping a fixed interval — is what makes E2E
suites flaky.

## The second implementation: `mbpoll` on libmodbus

`real_client_test.rs::test_modbus_reads_writes_and_exceptions_against_mbpoll` drives the real
`mbpoll` binary, which is C on libmodbus — a different language and a different stack from both
this server's hand-rolled codec and the `tokio-modbus` peer. It is **not** `#[ignore]`d, and it
**fails** rather than skipping when mbpoll is absent: a skip that returns `Ok(())` is a silent
pass on every machine without the binary, which is how a rating outlives its evidence.

Eight exchanges — every function code the server implements — each asserted on what libmodbus
*decoded and printed*:

| mbpoll invocation | FC | asserted |
|---|---|---|
| `-t 4 -r 0 -c 3` | 3 | the three register values, derived in the mock from the event's own `start_address`/`quantity` |
| `-t 0 -r 0 -c 4` | 1 | the coil pattern **in order** (`1 0 0 1`), so bit packing is checked rather than just the byte count |
| `-t 1 -r 0 -c 10` | 2 | ten discrete inputs in order, across the byte boundary four coils never reach |
| `-t 4 -r 7 <value>` | 6 | libmodbus validates the server's address+value echo before reporting the write |
| `-t 4 -r 20 1000 2000 65535` | 16 | accepted only if those three values reached the model; libmodbus checks the start+quantity echo |
| `-t 0 -r 3 1` | 5 | accepted, echo validated |
| `-t 0 -r 0 <nine bits>` | 15 | accepted only if the nine bits libmodbus packed reached the model in order |
| `-t 3 -r 0 -c 2` | 4 | exception 0x02 surfaced as a Modbus error, not as data |

Three things about the invocation are load-bearing:

- **`-1`, always.** Without it mbpoll polls forever, and every poll is another event and
  another mock call, so `expect_calls(1)` fails in a way that looks like a protocol bug.
- **`-0`** puts mbpoll into PDU addressing, so `-r 0` is literally address 0 on the wire and
  matches what the model is shown. mbpoll's default is 1-based and subtracts one.
- **No `unit_id` startup parameter.** With it unset the server answers every unit id, which is
  what lets mbpoll's default slave address 1 work untouched.

The exception case is the one that keeps the other three honest: without a negative path, a
server that answered every request with the same success frame would pass.
