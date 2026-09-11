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

Not verified against `etcdctl` or any other Go client. The gRPC status is
returned in the initial HEADERS rather than in HTTP/2 trailers — tonic accepts
that and grpc-go may not. Do not "fix" it without a Go client to test against.

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
