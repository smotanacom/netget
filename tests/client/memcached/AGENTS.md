# Memcached client tests

```bash
./cargo-isolated.sh test --no-default-features --features memcached --test client -- memcached --test-threads=100
```

| file | peer | LLM calls | proves |
|---|---|---|---|
| `real_server_test.rs` | real `memcached` + libmemcached `memcp`/`memcat` | 8 | the client bar (below) |
| `wire_test.rs` | none | 0 | `parse_reply` byte by byte, and every bound |
| `in_flight_test.rs` | a listener that never answers | 0 | the 128-request in-flight bound |

## real_server_test.rs — the evidence the rating rests on

`memcached` is spawned through `tests/helpers/real_server.rs` on a probed loopback port with
UDP off (`-U 0`) and `-vv`, and is ready when it logs `server listening`. It **fails, never
skips**, when `memcached`, `memcp` or `memcat` is missing. On Ubuntu libmemcached's tools ship as
`memccat`/`memccp`; CI's registry-audit job symlinks them to the upstream names.

`memcached_client_writes_and_acts_on_what_it_reads_against_memcached`:

1. `memcp` stores `prepared` = `written by memcp` (flags 7) and `binary` = four non-UTF-8 octets.
2. On `memcached_connected` the model sets `netget:greeting` = `hello from the model`, flags 42.
3. On that `memcached_stored` it `gets` `netget:greeting`, `prepared`, `binary`, `absent`.
4. Four events, each matched on its parsed fields: two `memcached_value`s (with `flags` and a
   `cas`), `memcached_error {kind: non_text_value, key: binary}`, `memcached_miss {key: absent}`.
5. From the `prepared` event the model sets `netget:echo` = `the model saw: written by memcp`.

Then `memcat` reads `netget:greeting` and `netget:echo`, and `memcat --verbose` must show
`flags: 42`. Condition 4 (the client acts on the model's answer) is asserted from the server's
side: emptying the loop over `result.actions` in `run_turns` fails this test.

`injected_memcached_actions_reach_memcached` drives the command channel in-process
(`AppState::send_to_client`, no model — the LLM endpoint is unreachable on purpose, so the connect
turn logs `decision=llm_error` and nothing else happens): a `memcached_set` that `memcat` reads
back (`Sent { bytes_sent: 45 }`), a `memcached_flush_all` without `confirm` that must come back
`Rejected`, and a `disconnect` that must leave the client `Disconnected` with no command handle.

## Mock rules

Every per-key rule is distinguished by `key` (and `value`/`flags` where it matters), so a reply
attributed to the wrong key fails the mock rather than answering the wrong rule. The two
`memcached_stored` rules are distinguished by `key`.

## Verified by mutation

With each of these removed, the named test failed: the loop over the model's actions
(`writes_and_acts`), the declared-size check, the line bound, the requested-key check, the STAT
count, the response-size bound, key validation (`wire_test`), the `confirm` gate
(`injected_memcached_actions_reach_memcached`), `MAX_IN_FLIGHT` (`in_flight_test`).
