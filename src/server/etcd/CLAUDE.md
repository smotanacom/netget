# etcd Protocol Implementation

etcd v3 KV service over gRPC. The handler (LLM, script, or static) answers every key-value
request; the server itself stores nothing.

**State**: `Experimental` · **Port**: 2379 · **Stack**: `ETH>IP>TCP>GRPC>ETCD`

## Libraries

- **prost** 0.13 — protobuf encode/decode. The etcd v3 schemas in `proto/etcd/` are compiled
  by `build.rs` into `etcdserverpb` / `mvccpb`.
- **hyper** 1.5 — `server::conn::http2` directly, with a hand-written gRPC router.
- **tonic** is a dependency of the `grpc` feature and is **not used here.** `metadata()` used
  to claim "tonic gRPC"; the routing, framing and status handling in `mod.rs` are all local.

## No storage (the rule that matters most here)

etcd is a key-value store, so this is the protocol most likely to grow one by accident. It has
not. The only server-side state is:

```rust
struct EtcdMeta {
    revision: i64,     // monotonic counter for the response header
    cluster_id: u64,   // constant
    member_id: u64,    // constant
}
```

A `kvs: HashMap<Vec<u8>, KeyValue>` field used to sit alongside these behind
`#[allow(dead_code)]`, never read and never written. It has been removed rather than left as a
half-built store. Keys and values live only in the request event and the handler's reply.

The revision counter is metadata, not data: it exists because `ResponseHeader.revision` is a
required field on every etcd reply and a client that sees it go backwards will complain. A
handler can override it on Put via `etcd_range_response`/`etcd_put_response`; the server only
ever raises it, never lowers it.

## Request flow

```
HTTP/2 stream -> route on :path -> decode protobuf -> emit event -> handler
             <- encode protobuf <- build response  <- actions   <-
```

Routed methods, all of `etcdserverpb.KV`:

| gRPC method | Event | Handler decides |
|---|---|---|
| `Range` | `etcd_range_request` | the key-value pairs returned |
| `Put` | `etcd_put_request` | the revision (optional) |
| `DeleteRange` | `etcd_delete_request` | how many keys were deleted |
| `Txn` | `etcd_txn_request` | whether the comparisons held |
| `Compact` | — | nothing; answered directly |

Anything else — `Watch`, `Lease`, `Auth`, `Cluster`, `Maintenance`, reflection — is answered
with `12 UNIMPLEMENTED`. It used to `bail!`, which reset the whole HTTP/2 connection and took
every concurrent RPC on it down with the unknown one.

## Actions

| Action | Fields | Used by |
|---|---|---|
| `etcd_range_response` | `kvs[]` (`key`, `value`, `create_revision`, `mod_revision`, `version`, `lease`), `more`, `count` | Range |
| `etcd_put_response` | `revision` (optional) | Put |
| `etcd_delete_range_response` | `deleted` | DeleteRange |
| `etcd_txn_response` | `succeeded` | Txn |
| `etcd_error` | `code`, `message` | any |

Every one of these is listed in `execute_action`. `etcd_put_response` and
`etcd_delete_range_response` previously were not: `handle_put` and `handle_delete_range` looked
for them by name while `execute_action` rejected them as unknown action types, so a model could
never produce one. A Put could not choose its revision and a DeleteRange always reported
`deleted 0` no matter what the handler said.

`etcd_error.code` maps onto a gRPC status: `KEY_NOT_FOUND`/`NOT_FOUND` → 5,
`INVALID_ARGUMENT`/`BAD_REQUEST` → 3, `UNIMPLEMENTED` → 12, `RESOURCE_EXHAUSTED` → 8,
`INTERNAL` → 13, anything else → 2 `UNKNOWN`. It was previously offered on the Range event and
then never looked for, so a handler reporting "key not found" had its answer silently dropped
and the client saw an empty success.

### Actions the model can see

All four request events call `.with_actions(...)`. `etcd_put_request`, `etcd_delete_request`
and `etcd_txn_request` used to call neither, and each carried an `etcd_range_response` as its
response example — the wrong action for the event. `call_llm` builds the model's tool list from
`event.event_type.actions`, so those three events fell back to the full sync set with a logged
BUG and a `debug_assert`.

## No bytes on the action boundary

etcd keys and values are `bytes` on the wire. They cross the action boundary as **UTF-8
strings**, converted with `from_utf8_lossy` inbound and `as_bytes()` outbound — never hex, never
base64. This satisfies the no-bytes rule and matches how anyone actually uses etcd (`etcdctl put
/config/db localhost:5432`), at the cost of being lossy for genuinely binary keys: a key
containing invalid UTF-8 reaches the handler with U+FFFD substitutions and cannot be echoed back
byte-exactly. Storing binary blobs in etcd through this server does not work.

The Txn comparison enums are given to the handler by name (`EQUAL`, `VERSION`, …) rather than as
the raw protobuf integers.

## Correlation

Request/response matching is HTTP/2's job, and hyper does it: each stream is one `service_fn`
future and hyper binds the returned `Response` to the originating stream id. There is no manual
stream bookkeeping to get wrong, and nothing correlation-related needs to appear in event data.

One consequence worth knowing: `connection_id` is minted per TCP connection, not per stream, and
gRPC multiplexes. Concurrent RPCs on one connection share it in the access log. The handlers
pass `None` for `connection_id` to `call_llm` anyway.

## Nothing the handler did not say

Every KV method refuses when its handler ran and produced no response action:
`Range`, `Put` and `DeleteRange` all reply `grpc-status 13 (INTERNAL)` with no
message frame, logged `decision=fail_closed_no_action`. INTERNAL rather than
UNAVAILABLE, because nothing is saturated and a retry would not help.

The reason this matters more here than in most protocols is that etcd's
"nothing" responses are **positive claims**, and so are indistinguishable from
real answers:

| method | what the old fall-through sent | how a client reads it |
|---|---|---|
| `Range` | `kvs: [], count: 0` | the key does not exist |
| `DeleteRange` | `deleted: 0` under a **bumped** revision | the delete ran and committed |
| `Put` | a `PutResponse` with a fresh revision | the write committed |

`Put` was fixed first; `Range` and `DeleteRange` carried the same shape for
longer. A handler that genuinely means "no such key" still says so — with
`etcd_range_response` carrying an empty `kvs` — and that is a different thing
from having produced no action at all.

`Txn` is the one method that does not refuse, deliberately: it defaults
`succeeded: false`, which is the safe direction (a compare that did not hold, so
a distributed lock is not acquired) rather than a claim about the key space.

`tests/server/etcd/unanswered_request_test.rs` drives `Range` and `DeleteRange`
through a zero-action static handler — the shape that reaches the response
builder with nothing and no backend error to blame — and asserts a non-zero
status and an empty body.

## Robustness

- **Body size is capped** at 1.5 MiB (`MAX_REQUEST_BYTES`, matching etcd's own
  `--max-request-bytes` default) via `http_body_util::Limited`. `req.collect()` was previously
  unbounded — HTTP/2 flow control bounds the window, not the total, so one client could grow
  the process without limit.
- **The 5-byte frame header is honoured.** The declared length is checked against the bytes
  that actually follow before slicing; the code used to take "everything after byte 5",
  feeding a second frame's bytes into the protobuf decoder as trailing garbage.
- **A set compression flag is rejected** with `12 UNIMPLEMENTED` instead of being ignored and
  passed to prost as if it were uncompressed.
- **No handler returns `Err` to hyper.** Every failure becomes a well-formed gRPC status reply.
  An error out of `service_fn` resets the whole multiplexed connection.
- **`grpc-message` is built with `HeaderValue::from_str` and a fallback**, not `unwrap`. The
  message comes from LLM output and `anyhow` chains; a single non-ASCII character in it is
  illegal in a header value and would otherwise panic the connection task.
- **The accept loop breaks** on error instead of retrying immediately, which spun a hot loop
  on a persistent EMFILE and flooded the unbounded status channel.
- Bind uses `?`; the accept-loop `JoinHandle` is registered via `register_server_task()`, so
  `stop_server` releases the port. Per-connection tasks are untracked (project-wide gap).

There are no `unwrap()`s, no slicing, and no signed-to-`usize` casts on network input left in
this module.

## Known limitations

- **KV only.** No Watch (needs server streaming), no Lease, no Auth, no Cluster, no
  Maintenance, no reflection.
- **Txn is partial.** The handler decides `succeeded`; the nested Range/Put/Delete operations
  inside the success and failure branches are not executed, so `responses` is always empty. A
  client using `Txn` purely as compare-and-swap (the distributed-lock pattern) works; one that
  reads a value out of the transaction result does not. Before this, `handle_txn` never called
  the handler at all and hardcoded `succeeded: false`, so every lock acquisition failed.
- **`Compact` is a stub** — it acknowledges without consulting the handler. Nothing is stored,
  so there is nothing to compact.
- **No MVCC.** One revision counter, no history, no point-in-time reads. Earlier docs described
  "simplified MVCC" and a `kvs` map; neither exists.
- **No persistence, no Raft, single node.**
- Startup parameters: only `cluster_name` (a log label). `initial_cluster_state` and `max_keys`
  used to be declared and were never read — `max_keys` in particular advertised a key store
  this protocol must not have.

## Verified

**State: Beta**, and the evidence holds up to the usual checks:
`tests/server/etcd/e2e_test.rs` drives the real `etcd_client` crate (tonic-based) through
put / get / range / delete against the mocked LLM; it is not `#[ignore]`d, it does not skip
when anything is missing, and `etcd-client` is a plain optional dependency the `etcd` feature
turns on, so it compiles wherever the feature does — not an optional dev-dependency that the
blocking CI job would never build. Not verified against `etcdctl`
or any other Go client: the gRPC status is returned in the initial HEADERS rather than in
HTTP/2 trailers, which tonic accepts and which grpc-go may not. Do not "fix" that without a Go
client to test against — moving the status into trailers would change how tonic classifies the
response and could break the path that currently works.

## References

- [etcd v3 API](https://etcd.io/docs/v3.5/learning/api/)
- [etcdserverpb](https://github.com/etcd-io/etcd/tree/main/api/etcdserverpb)
- [gRPC over HTTP/2](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md)
