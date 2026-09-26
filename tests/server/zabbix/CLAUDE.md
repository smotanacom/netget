# Zabbix tests

Run everything:

```bash
./cargo-isolated.sh test --no-default-features --features zabbix --test server -- zabbix:: --test-threads=100
```

## Strategy

The evidence is `real_client_test.rs`, which points the Zabbix project's own `zabbix_sender` at
the server and asserts on the counts it printed **and its exit status** — the sender scans our
`info` string to choose 0 or 2, so a misrendered string shows up as the wrong exit code. It also
records one exchange through a relay and runs the pcap oracle with Wireshark's `zabbix`
dissector. Everything else is NetGet reading bytes NetGet wrote.

Most suites start the server **in process** through `ServerForm` with a static or Python script
handler and a dead model endpoint. `e2e_test.rs` and one case in `real_client_test.rs` use the
spawned binary with the mock model.

## Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process server, `exchange` (one request, read to EOF), `response` (header + JSON), `TRAPPER_SCRIPT` (values whose key starts with `bad` fail), the recording relay | — |
| `real_client_test.rs` | `zabbix_sender -s -k -o` exits 0 with `processed: 1; failed: 0; total: 1` and the pcap oracle reads the exchange clean; `-i` with three values, one rejected, exits 2 with `processed: 2; failed: 1; total: 3`; a dead backend makes it exit 2 with `processed: 0; failed: 1`; and a **mocked model** decides a batch. Fails, never skips, without `zabbix_sender`. | 2 (mocked case) |
| `e2e_test.rs` | mocked model sees items/values/`item_count`; exact response shape and the info regex; the large header accepted; empty batch, `active checks`, non-JSON, bad `data`, 1001 values answered by NetGet with no model call (`expect_calls`) | 3 |
| `wire_test.rs` | proptests: both header forms round-trip; any declared length past 1 MiB refused in either form; `render_result`/`read_result` round-trip and the info matches zabbix_sender's `sscanf` shape; parsers never panic. Tables: header refusals (magic, flags, compressed), zabbix_sender 7.4's own captured request, a 100,000-level nesting bomb refused by serde_json's limit, the `failed` shape | 0 |
| `connection_bounds_test.rs` | exactly 1 MiB answered; a header declaring 1 MiB + 1, and a large header declaring 2^40, refused `message is too large` with no body sent and no handler; the refusal survives 48 KiB of unread body (`linger`); compressed, unknown flags, non-ZBXD; first-byte deadline; idle deadline mid-request (a different number); a `manual`-parked request outlives both; the 257th connection closed with no bytes, slot returns | 0 |
| `llm_failure_test.rs` | dead backend → `processed: 0; failed: 2` + `decision=fail_closed_llm_error`, no leaked error text; empty handler → same + `model_silent`; `processed: 5` for two values → same + `fail_closed_mismatched_reply`; model `processed: 0` → `model_reject`; `close_connection` → no bytes + `model_close` | 0 |
| `peer_inject_test.rs` | a parked request answered from `send_to_peer` with the executor's rendering; `close_connection` reaches the sender as EOF | 0 |

## How each guard was shown to matter

Removed together in one mutated build, then restored (`linger` separately):

| Guard removed | Test that failed, and how |
|---|---|
| the declared-length check in `parse_header` | `a_declared_length_past_the_limit…` (no answer in 10 s — waiting for a body never sent), the proptest (1 MiB + 1 accepted) |
| the compressed-flag refusal | `compressed_unknown_flags…` (no answer in 10 s), `header_refusals` |
| `MAX_ITEMS` | e2e (1001 values answered `processed: 1001`) |
| read deadlines (→ 3600 s) | `a_peer_that_says_nothing…`, `a_peer_that_stalls_mid_request…` (never closed) |
| the connection cap (×1000) | `the_connection_past_the_cap…` (over-cap peer neither answered nor closed) |
| the counts check (`t == total`) | `counts_that_do_not_add_up…` (`processed: 5; failed: 0; total: 2` reached the sender) |
| the fail-closed counts (backend failure answered `processed: N` instead) | `a_backend_failure…` in both suites — **zabbix_sender exited 0** |
| `linger` | `the_too_large_refusal_survives…`: `ConnectionReset`, 2 of 4 runs |

## Notes

- `zabbix_sender` is at `/opt/homebrew/bin/zabbix_sender` here (`brew install zabbix`); CI's
  `registry-audit` installs Ubuntu's `zabbix-sender` and runs `zabbix::real_client_test`.
- zabbix_sender's `-i` input format is `<host> <key> <value>` per line (`-` for the `-s` host);
  with `-T` a timestamp column precedes the value.
