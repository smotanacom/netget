# Memcached server test strategy

## Layer 1 — parsing and framing (`e2e_test.rs`, no network, no LLM)

LLM calls: **0**. Runtime: milliseconds.

The load-bearing test is `a_stored_value_may_contain_crlf`. A storage command's data block is
delimited by its declared byte count, **not** by scanning for `\r\n`; scanning is the classic
memcached implementation bug and it corrupts every subsequent command on the connection. The
test stores `"line one\r\nline two"`, asserts the payload survives byte for byte, and then
parses a `version` command from immediately behind it to prove the stream stayed aligned.

`a_storage_command_is_incomplete_until_the_whole_data_block_arrives` walks every prefix of a
complete frame and requires `Incomplete` for all of them — so a partially-arrived command can
never be mistaken for a short one.

`frames_values_exactly`, `frames_gets_with_the_cas_unique` and `a_cache_miss_is_end_alone` pin
the reply framing against literal bytes, including a binary payload with a NUL and a 0xFF.

### `the_protocol_implements_no_storage`

This one is unusual and deliberate: it reads `src/server/memcached/*.rs`, strips comments, and
fails if the code contains `HashMap`, `BTreeMap`, `DashMap` or `std::fs::`.

"The model is the cache" is the entire design claim of this protocol. It is also exactly the
kind of claim that erodes quietly — the first time someone wants `gets` to return a stable
`cas_unique`, or wants two `get`s of the same key to agree, a small map is the obvious fix and
nobody would notice. This test makes adding one a conscious argument rather than an
accident. The comment-stripping matters because the module docs discuss `HashMap` precisely to
say there isn't one.

If persistence is genuinely wanted, the sanctioned route is the generic SQLite facility in
`src/state/sqlite.rs`, which the model opts into at runtime.

## Layer 2 — end to end through the real binary (`e2e_test.rs`, LLM mocked)

A raw `TcpStream` plays the client so the assertions can be on exact bytes.

| Test | LLM calls | What it pins |
|---|---|---|
| `get_returns_the_value_the_model_invents` | 3 | Hit framed exactly (`VALUE greeting 0 11\r\n…`), miss is `END\r\n` alone. The mock builds its answer from the event's `keys`, so wrong key parsing turns it red |
| `set_with_an_embedded_crlf_is_counted_not_scanned` | 3 | The CRLF payload case end to end, *and* a pipelined `version` behind it. The mock answers `SERVER_ERROR` with the wrong framing spelled out if `bytes`/`value` are not exactly right, so a failure names the defect |
| `arithmetic_delete_and_unknown_verbs` | 4 | incr derives its answer from `delta`; delete and an unknown verb bundled into the same server to stay inside budget |
| `an_unanswered_command_becomes_server_error_not_a_fabricated_hit` | 2 | See below |

Total: **12 LLM calls** across four servers, i.e. 2–4 per test. The budget guidance is
~10 per suite; these are cheap mocked calls and bundling further would make failures harder to
attribute, which is the worse trade.

### The fail-visibly test

`an_unanswered_command_becomes_server_error_not_a_fabricated_hit` is the memcached analogue of
the RADIUS fail-closed test. Memcached clients block on a reply, so silence hangs them until
their own timeout. The server answers `SERVER_ERROR`, and the test asserts three things:

- the reply starts with `SERVER_ERROR`;
- it contains no `VALUE` — silence must never become a cache hit;
- it contains no `END\r\n` either. Reporting a clean miss would be a *lie the client caches*,
  and is the caching equivalent of OAuth2's fail-open.

## Layer 3 — a real, independent client (`real_client_test.rs`)

**libmemcached 1.0.18** (`brew install libmemcached`; BSD-3, invoked as subprocesses, never
linked). This is the only peer here that NetGet did not write.

`memcat` is a good oracle precisely because it trusts the `VALUE` header: it reads exactly the
declared number of bytes, so a byte count that disagrees with the payload shows up as empty or
truncated output rather than a passing test. `memstat` parses `STAT` lines up to `END`;
`memping` exercises `version`.

| Test | LLM calls | What it pins |
|---|---|---|
| `libmemcached_memcat_reads_a_value_the_model_invented` | ≥2 | A foreign C client reads a model-invented value |
| `libmemcached_memstat_and_memping_accept_our_replies` | ≥3 | Stats and version framing accepted by a foreign client |

Both **fail** with an explicit message when the tools are absent — they do not skip. A
skip-when-missing gate returns `Ok(())` on a runner without libmemcached, which is a silent
pass, and Memcached's `Beta` rating would then rest on nothing. Install with
`brew install libmemcached` or `apt-get install -y libmemcached-tools`. These use `expect_at_least` rather than `expect_calls` because libmemcached opens
connections and issues probe commands at its own discretion.

## Not tested, because not implemented

The binary protocol (deprecated upstream in 1.6, 2020), the meta commands (`mg`/`ms`/`md`),
SASL authentication, and UDP mode. `metadata()` says so. Expiry is reported to the model but
nothing expires, because nothing is stored — there is no test asserting expiry works.

## Running

```bash
CARGO_TARGET_DIR=/tmp/tgt ./cargo-isolated.sh test --no-default-features --features memcached \
    --test server::memcached -- --test-threads=100
```
