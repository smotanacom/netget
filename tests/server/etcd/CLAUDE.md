# etcd server tests

Four files, all declared in `tests/server/etcd/mod.rs`:

| file | what it is for | LLM calls |
|---|---|---|
| `e2e_test.rs` | the maturity evidence — the real `etcd_client` crate driving Put / Get / Range / Delete | ~6 |
| `llm_failure_test.rs` | which gRPC status a backend failure produces, and why it matters which | 1 |
| `unanswered_request_test.rs` | a handler that produces no action must not be answered for | 1 |
| `connection_tracking_test.rs` | connections reach `AppState` and close again | 0 |

```bash
./cargo-isolated.sh test --no-default-features --features etcd \
    --test server -- server::etcd --test-threads=100
```

`etcd` needs `protoc` to build (`build.rs` compiles the etcd protobuf schemas
under `#[cfg(feature = "etcd")]`), so this suite does not run in Claude Code for
Web and is outside the blocking CI feature set.

## The maturity evidence, and why it holds

`e2e_test.rs` drives **`etcd-client` v0.15**, the official Rust client, over
tonic. That is what `DevelopmentState::Beta` rests on, and it survives the three
checks this repo applies to such a claim:

- it is **not** `#[ignore]`d;
- it does **not** skip when something is missing — there is no
  `SKIP: … not installed` path, so it cannot silently pass;
- `etcd-client` is a plain optional dependency the `etcd` feature turns on, not
  an `optional = true` **dev**-dependency. That distinction is what makes AMQP's
  Beta rest on a test its gate never compiles; etcd does not have that hole.

**That evidence is real and it is no longer enough — the rating is now
`Experimental`.** This file used to say the gRPC status is returned in the
initial HEADERS rather than in HTTP/2 trailers, that tonic accepts it and
"grpc-go **may** not", and that nobody should touch it without a Go client to
test against. September 2026: a Go client was pointed at it, and grpc-go does
not.

```
$ etcdctl --endpoints=http://127.0.0.1:PORT put /config/database localhost:5432
Error: rpc error: code = Internal desc = server closed the stream without sending trailers
```

Not a get, not an edge case — the **first** RPC, and every other one that
carries a body. gRPC-over-HTTP/2 requires `grpc-status` in Trailers whenever a
response has a body; `src/server/etcd/mod.rs` writes it into the initial HEADERS
and then sends DATA, so the stream ends after DATA with no trailing HEADERS.
Error replies are unaffected because they are Trailers-Only by construction,
which is exactly why no existing test caught this: only the success path is
broken.

So the Beta rating rested on tonic being lenient about a thing the reference
implementation is strict about — the same shape as `mysql`'s Beta resting on
`mysql_async` where the real `mysql` CLI cannot connect at all. One client
agreeing is not "works against real clients".

**No `etcdctl` test is committed.** A test asserting this failure would lock the
bug in and turn its fix into a red build. The fix is to emit real HTTP/2
trailers on the success path; once that lands, an `etcdctl` put/get/prefix/del
test is the promotion evidence, and it is about eighty lines.

## No Ollama, and no store

An earlier version of this file said Ollama must be running and described a
prompt telling the server to "store it in memory with revision tracking". Both
are wrong, and the second is wrong in a way worth naming:

- **The mock is the model.** Every test here uses `NetGetConfig::with_mock`, an
  in-process axum server. Nothing contacts a real backend.
- **etcd stores nothing, by rule.** `EtcdMeta` holds a revision counter, a
  cluster id and a member id — no keys. A `kvs: HashMap` field once sat there
  behind `#[allow(dead_code)]`; it is gone rather than left as a half-built
  store, and `max_keys` was removed from the startup parameters because it
  advertised exactly the store this protocol must not have. A test that expects
  a Get to return what an earlier Put wrote is expecting the **mock** to be
  consistent, not the server.

## Mock rules

`e2e_test.rs` uses one rule per etcd event with
`respond_with_actions_from_event`, which is what lets a Get after a Delete answer
`count: 0` while the earlier Get answers with the value. Two rules on the same
event with nothing to tell them apart is the most common mock mistake in this
repo: the first answers every occurrence and the second reports zero calls.

Event ids are `etcd_range_request`, `etcd_put_request`, **`etcd_delete_request`**
(not `etcd_delete_range_request` — the action is
`etcd_delete_range_response`, the event is not) and `etcd_txn_request`. Check
them against `src/server/etcd/actions.rs`, because a rule that never matches does
not fail: the request falls through to a real LLM call and the failure surfaces
two steps later on a different expectation.

Always finish with `wait_for_mocks(30)` then `verify_mocks()`. Without the
second, a test asserts nothing at all about LLM interaction.

## What is not covered

- **Watch, Lease, Auth, Cluster, Maintenance** — the server answers all of them
  `12 UNIMPLEMENTED`; Watch needs server streaming.
- **Txn's nested operations.** The handler decides `succeeded`; the Range/Put/
  Delete operations inside the branches are not executed, so `responses` is
  always empty. Compare-and-swap (the distributed-lock pattern) works; reading a
  value out of a transaction result does not.
- **Concurrency, large values, many keys.**
