# etcd Client E2E Test Documentation

Three files, declared in `tests/client/etcd/mod.rs`. Nothing is `#[ignore]`d and nothing needs
Docker.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **the official Go `etcd`** + `etcdctl` | 1 | 6 |
| `e2e_test.rs` | NetGet's own etcd server | 3 | 18 (7 + 8 + 3) |
| `command_channel_test.rs` | NetGet's own etcd server, in-process | 1 | 0 |

```bash
./cargo-isolated.sh test --no-default-features --features etcd --test client -- etcd:: --test-threads=100
```

`protoc` must be installed: the `etcd` feature's `build.rs` compiles the server's protos.

## `real_server_test.rs` — the evidence the rating rests on

NetGet's client is `etcd-client` (Rust, tonic). The server is the official `etcd` (Go,
grpc-go), started per test by `tests/helpers/real_server.rs` as a single member with client and
peer URLs on `http://127.0.0.1:0`; the client port is read back from etcd's own
`serving client traffic insecurely … "address":"127.0.0.1:N"` log line (3.4's
`serving insecure client requests on …` is accepted too), so there is no probe-port race.
State is read back with the official `etcdctl`. **It fails, never skips,** when either binary
is missing.

### `etcd_client_puts_gets_and_deletes_against_the_official_etcd` (6 calls)

`etcd_connected` → `etcd_put netget/greeting`; its response (operation `put`, key
`netget/greeting`) → `etcd_get`; the get response (operation `get`, value present in `kvs`) →
`etcd_put netget/echo = "the model saw: <value> at version <version>"`, built from the `kvs`
entry etcd returned; that put's response → `etcd_delete netget/greeting`; the delete response
(`deleted` 1) → nothing. Then `etcdctl get netget/echo --print-value-only` must print
`the model saw: hello from the model at version 1`, and `netget/greeting` must be gone.

**Condition 4 of the client bar**: the value `etcdctl` reads was composed by the model from a
response it was shown. Verified by mutation: dropping the actions in `notify_response`'s loop
makes the test fail while the mock still records calls.

No sleeps: the last rule is the delete's response, so when `wait_for_mocks` returns, etcd has
applied everything the model sent.

## `e2e_test.rs` — same-project

`test_etcd_client_basic_operations` (7 calls), `test_etcd_client_multiple_keys` (8) and
`test_etcd_client_nonexistent_key` (3) drive the client against NetGet's own etcd server, each
side with its own mock. That is the circular case for *client* evidence — the two halves were
written to agree — so these are kept for what they add (multiple keys, a missing key, both
mocks asserted) and are not what the rating rests on.

## Not covered

Range/prefix reads, transactions, watches, leases, authentication and TLS: the client offers
the model `etcd_get`, `etcd_put` and `etcd_delete` on single keys only.

## `command_channel_test.rs` — injected actions

In-process, no NetGet subprocess and **zero LLM calls**: a NetGet etcd *server* answers
through static handlers (`etcd_put_request` → `etcd_put_response{revision: 7}`,
`etcd_range_request` → one kv) and the client's LLM points at `http://127.0.0.1:1`, so its
`etcd_connected` call fails and the connect path has to tolerate that.

What it pins:

- `has_client_handle` is true **before** anything answers the connected event.
- `etcd_put` → `Executed { detail }` carrying the key and the revision *our server* returned,
  and the server's access log shows the key — so the operation really crossed the wire.
- a second `etcd_get` on the same client succeeds, which is what proves the session is held
  rather than re-dialled per operation.
- an unknown action → `Rejected`; `disconnect` → `Disconnected`, status `Disconnected`,
  handle gone.

`Executed`, not `Sent`: `etcd-client` owns the HTTP/2 connection and reports no byte counts.

**LLM call budget: 0.**
