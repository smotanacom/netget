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

**State: Experimental**, demoted from Beta in September 2026 by the Go client this file
used to say nobody should proceed without.

The Rust evidence is intact and worth keeping: `tests/server/etcd/e2e_test.rs` drives the real
`etcd_client` crate (tonic-based) through put / get / range / delete against the mocked LLM;
it is not `#[ignore]`d, it does not skip when anything is missing, and `etcd-client` is a plain
optional dependency the `etcd` feature turns on, so it compiles wherever the feature does — not
an optional dev-dependency that the blocking CI job would never build.

**What changed is that the caveat stopped being hypothetical.** This file said the gRPC status
is returned in the initial HEADERS rather than in HTTP/2 trailers, that tonic accepts it and
grpc-go "may not", and that it should not be touched without a Go client to test against.
A Go client was tried:

```
$ etcdctl --endpoints=http://127.0.0.1:PORT put /config/database localhost:5432
Error: rpc error: code = Internal desc = server closed the stream without sending trailers
```

grpc-go cannot complete a **single** RPC that carries a body — not a corner of the API, the
first Put. The spec requires `grpc-status` in Trailers for any response with a body;
`grpc_status_reply` and the success path both put it in the initial HEADERS, and the success
path then writes a `Full<Bytes>` DATA frame, so the stream ends with no trailing HEADERS. The
error paths survive only because an empty body makes them Trailers-Only by accident, which is
why every test in the tree passes.

That makes the Beta rating an instance of one lenient client agreeing with one bug — the same
shape CLAUDE.md records for `mysql`/`mysql_async`. Hence `Experimental`.

**The fix, when someone takes it**: emit real HTTP/2 trailers on the success path. The concern
this file raised — that moving the status would break tonic — is worth testing rather than
assuming, because trailers are what tonic expects too; the header placement is the non-standard
one. Keep the Trailers-Only shape for errors, which is already correct. Then add the `etcdctl`
test: put, get, `get --prefix` (three pairs, so repeated fields are exercised) and `del`,
asserting on what `etcdctl` printed, hard-failing when the binary is absent. That test is
written and was thrown away rather than committed red; it is roughly eighty lines.

## References

- [etcd v3 API](https://etcd.io/docs/v3.5/learning/api/)
- [etcdserverpb](https://github.com/etcd-io/etcd/tree/main/api/etcdserverpb)
- [gRPC over HTTP/2](https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md)

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever, and a
hundred of them was a free denial of service on a server that would happily accept a hundred
more. It now declares both halves; the constants and the reasoning live beside them in
`src/server/etcd/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | Enforced with `TcpStream::peek` before the socket reaches hyper, so the HTTP/2 preface is still there afterwards. HTTP/2 is client-speaks-first and every gRPC client sends the preface inside its dial path. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 900s | etcd's `--grpc-keepalive-interval` defaults to two hours, a bound in name only, so there is no upstream number worth copying. Fifteen minutes is safe here because of a property of *this* server: every RPC is unary — `handle_grpc_request` returns a `Response<Full<Bytes>>`, so even Watch is one complete message rather than a held-open stream. There is no legitimate long-lived silent request. |
| `MAX_CONNECTIONS` | 256 | Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After`**, deliberately in the older protocol — a refused peer has not sent the HTTP/2 preface, so nothing has been negotiated and a GOAWAY would have to follow a SETTINGS exchange this server is declining. |

**The deadline covers the read and nothing else.** hyper owns every read once `serve_connection` starts, and it keeps polling the connection for new frames *while a request is being answered* — so a deadline on reads would be wrong here, not merely awkward. The idle bound is a watchdog over `ConnectionActivity` instead, which reports a connection with work in flight as not idle at all. The LLM round-trip, and a `manual`
rule parking an event for a human (`src/state/intercepts.rs`, 300s by default), are outside
every deadline here, so an answer that takes minutes can never close the connection it is an
answer for. That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse:
TFTP evicted live transfers because "idle" was measured wrongly.

`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is removed;
`tests/accept_bounded_test.rs` drives the shared helper, including the guarantee that a busy
connection is never reported as idle.

## No peer handle — `[ message this peer ]` / `[ disconnect this peer ]` stay disabled

etcd is gRPC over HTTP/2, and `handle_connection` hands the socket to hyper's
`serve_connection` after the first-byte `peek`. From that point hyper owns every read and write,
so there is no `Arc<Mutex<WriteHalf>>` to share with a peer-command task, and raw bytes written
beside hyper's framing would corrupt the HTTP/2 stream rather than reach the client.

What the protocol *could* offer is also nothing: `EtcdProtocol`'s actions return
`ActionResult::Custom`, which `mod.rs` turns into a protobuf reply for the RPC that is in
flight. An injected one has no RPC to answer.

The dashboard renders that as a dim button reading "this protocol cannot message a peer from
here yet", which is the honest rendering. `tests/peer_handle_coverage_ratchet_test.rs` carries
this protocol on its shrink-only baseline with the reason above, and re-derives the reason from
source on every run, so if the mechanism changes the build fails rather than the file going
quietly stale.
