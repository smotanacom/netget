# Zabbix Trapper Implementation

The trapper side of the Zabbix protocol — what `zabbix_sender` talks to on a Zabbix server or
proxy (port 10051). The model is the server's item processing: it sees the values a sender
reported and decides how many it accepts. NetGet renders every byte of the response, including
the `info` string whose counts decide `zabbix_sender`'s exit status.

**State**: Experimental (see Maturity). **Privilege**: `None` — the well-known port is 10051.
**Stack**: `ETH>IP>TCP>Zabbix`. **Feature**: `zabbix` (no dependencies beyond serde_json).

## Library choice

None. The framing is a 13- or 21-byte header in front of JSON; `wire.rs` parses and renders it
in ~250 lines of pure functions and serde_json does the JSON.

## Files

| File | What it holds |
|---|---|
| `mod.rs` | accept loop, the one-request session, the model call and the counts check |
| `wire.rs` | `parse_header`/`header_len`/`encode`/`encode_large`, `parse_request`, `render_result`/`render_failed`/`read_result`/`info_string`, the bounds |
| `actions.rs` | the `Protocol`/`Server` impls, `send_zabbix_result` and `close_connection`, the `zabbix_sender_data` event |

## Wire format and spec subset

`"ZBXD"` + flags + data length + reserved, then the data. Flags: `0x01` required, `0x04` means
8-byte length and reserved fields (both accepted), `0x02` means zlib-compressed data with the
uncompressed size in `reserved` — **refused** (`compressed data is not supported`).
`zabbix_sender` 7.4 sends flags `0x01` and no compression (measured against a capture listener),
so refusing costs the sender nothing and removes the only path where the bytes read and the
bytes processed differ (a zip bomb).

| Request | Answered by | Response |
|---|---|---|
| `{"request":"sender data","data":[{host,key,value,clock?,ns?}…],"clock"?,"ns"?}` | model → `zabbix_sender_data {items, item_count, clock?}` | `{"response":"success","info":"processed: P; failed: F; total: T; seconds spent: S"}` |
| the same with an empty `data` | NetGet | `processed: 0; failed: 0; total: 0` |
| any other `request` (`active checks`, `agent data`, `zabbix.stats`, …) | NetGet | `{"response":"failed","info":"unsupported request"}` |
| not a JSON object, no `request`, `data` not an array of `{host,key,…}` | NetGet | `failed` with a fixed `info` |
| more than `MAX_ITEMS` values | NetGet | `failed`, `too many values in one request` |
| not `ZBXD` at all | NetGet | nothing: the connection closes (`decision=fail_closed_bad_header`) |

One request per connection, then close — as the Zabbix server does and as `zabbix_sender`
expects. Values reach the model as text (numbers and booleans stringified), `clock`/`ns` as
integers when present. Nothing is stored.

`zabbix_get` is not served: it talks to an *agent* (port 10050, passive checks), a different
request set.

## What the model sees and controls

| Action | Renders |
|---|---|
| `send_zabbix_result {processed, failed}` | the `success` response; `seconds spent` is the real time since the request started arriving |
| `close_connection` | close without a response (`decision=model_close`); zabbix_sender reports a failed send |

The loop reads the counts back out of the rendered packet (`wire::read_result`) and requires
`processed + failed` to equal the request's own value count. A mismatch is refused (below).

## Failure behaviour

`FailureMode::Answers`. Every failure after a valid request is answered
`processed: 0; failed: N; total: N`, which makes `zabbix_sender` exit **2**:

| Cause | Log token |
|---|---|
| backend failed | `decision=fail_closed_llm_error category=unavailable\|overloaded` |
| the handler/model answered nothing | `decision=model_silent` |
| counts that do not add up to the request | `decision=fail_closed_mismatched_reply` |

**Why not `{"response":"failed"}`**: zabbix_sender 7.4 prints `Warning: incorrect answer` on it
and exits **0** (measured), so a script checking `$?` would believe its values were stored —
fail-open at the client. Counting every value failed is the one answer it acts on. The two
backend categories cannot be told apart on the wire without breaking the `info` format, so only
the log carries them. A model `processed: 0` is `decision=model_reject`; anything else
`decision=model_answer`. No error text reaches the peer: `info` is either rendered from numbers
or one of `wire.rs`'s fixed strings.

## Bounds

| Bound | Value | Override | Why |
|---|---|---|---|
| `MAX_DATA_BYTES` (= `max_inbound_bytes`) | 1 MiB | — | The protocol allows 1 GiB (`ZBX_MAX_RECV_DATA_SIZE`), a limit for a server streaming into a database. Here every byte becomes a model prompt, and zabbix_sender batches at most 250 values per request, so 1 MiB is ~4 KiB per value. Judged from the **declared** length in the header (either size) before anything is allocated; over it: `failed`/`message is too large` and close, `decision=fail_closed_too_large`. |
| `MAX_ITEMS` | 1000 | — | Four times zabbix_sender's batch of 250. |
| JSON nesting | serde_json's 128 | — | A nesting bomb is a parse error (`cannot parse request as a JSON object`), not a stack overflow; the fuzz target seeds 65,536 levels. |
| `FIRST_BYTE_TIMEOUT` | 30 s | `first_byte_timeout_secs` | zabbix_sender writes at once (the Zabbix server's own `Timeout` is 3 s). |
| `IDLE_TIMEOUT` | 30 s | `idle_timeout_secs` | Between reads of a request that has started arriving. |
| `MAX_CONNECTIONS` | 256 (house default) | — | The peer past the cap is closed with **no bytes**: a response to a request it has not sent would be read as the answer to it. |

The session reads exactly what the header asks for, so a refused request usually leaves input
unread; **every close lingers** (2 s / 64 KiB) so the refusal is not destroyed by an RST.
Removing `linger` makes `the_too_large_refusal_survives…` fail with `ConnectionReset` in 2 of 4
runs. The deadlines wrap the reads only, so a request parked for a human under a `manual` rule
is closed by neither.

## Peer handle

Registered at connect, so a request parked for a human can be answered from the dashboard's
`[ message ]` (`send_zabbix_result` renders the same packet, with `seconds spent: 0.000000`
because the executor has no clock for the request) or `[ disconnect ]`.

## Wireshark

`zabbix` is Wireshark's own dissector (`tshark -G protocols` lists it; decode-as
`tcp.port==N,zabbix`). `real_client_test.rs` runs the pcap oracle over a real zabbix_sender
exchange recorded through a relay, and it is clean.

## Maturity

Experimental. The evidence for Beta is in place — `tests/server/zabbix/real_client_test.rs`
drives the Zabbix project's own `zabbix_sender` 7.4 (C; not linked; the server uses no Zabbix
library) and asserts on its printed counts and its exit status, including exit 2 on a backend
failure; the Wireshark dissector reads the recorded exchange clean; it is not `#[ignore]`d and
fails, never skips, without the binary; CI's `registry-audit` installs Ubuntu's `zabbix-sender`
and runs it. Promotion is a separate step.
