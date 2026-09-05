# MongoDB Server Implementation

MongoDB wire protocol server. There is no Rust MongoDB *server* library, so
OP_MSG is parsed and built by hand; `bson` v3.0 only does document
encoding/decoding. **There is no storage** — no collections, no documents, no
in-memory map. The LLM answers every command except the `hello`/`isMaster`
handshake, which the server answers itself.

**State**: Experimental — LLM-authored, not human-reviewed. Verified against the
official `mongodb` driver (the full E2E suite passes) and against a raw socket
for the malformed-header paths.
**Port**: 27017 by default. **Privilege**: `None` (27017 > 1024).
**Stack**: `ETH>IP>TCP>MongoDB`.

## What the model sees and controls

**Events**:

`mongodb_command`, one per OP_MSG:

| Field | Notes |
|---|---|
| `command` | the first key of the command document — `find`, `insert`, `hello`, … |
| `database` | the `$db` field, or `admin` |
| `collection` | the *value* of the command key (`{find: "users"}` ⇒ `"users"`), or null |
| `filter` | the `filter` sub-document as relaxed extended JSON, or null |
| `document` | the `documents` or `document` field as relaxed extended JSON, or null |

`mongodb_disconnected`, once when the socket closes:

| Field | Values |
|---|---|
| `reason` | `client_disconnect`, `close_this_connection`, `invalid_message_length`, `unsupported_opcode`, `malformed_op_msg` |

The socket is dropped before this event fires, so the LLM round-trip does not
hold the connection open.

**Actions** (all sync; there are no async actions). These are attached to
`MONGODB_COMMAND_EVENT` via `.with_actions(...)` — `call_llm` builds the model's
available-action list from the *event type*, not from `get_sync_actions()`, so an
event without that list rejects everything the model produces as an unknown
action.

| Action | Parameters | Reply document |
|---|---|---|
| `find_response` | `documents` (required) | `{ok: 1, cursor: {id: 0, ns: "<db>.<collection>", firstBatch: […]}}` |
| `insert_response` | `inserted_count` | `{ok: 1, n: …}` |
| `update_response` | `matched_count`, `modified_count` | `{ok: 1, n: …, nModified: …}` |
| `delete_response` | `deleted_count` | `{ok: 1, n: …}` |
| `error_response` | `code` (required), `message` (required) | `{ok: 0, code: …, errmsg: …}` |
| `close_this_connection` | — | closes the connection |

`ns` is built from the request's own `$db` and collection. It used to be
hardcoded to `test.collection` regardless of what the client asked for.

Documents are given as JSON and converted with `Bson::try_from`; use MongoDB
extended JSON for non-JSON types (`{"_id": {"$oid": "507f1f77bcf86cd799439011"}}`).
An element that will not convert is silently dropped from the batch.

### Failure behavior

MongoDB is strictly request/response: a command with no reply hangs the driver
until its own timeout, which it then reports as a *network* fault — sending a
replica-set-aware driver looking for another node instead of surfacing a server
error. So every failure answers, and every answer carries a category only. The
error itself goes to `tracing::error!` and the status stream; nothing derived
from it reaches the socket (`crate::utils::WireFailure`, `&'static str` by
construction).

Four outcomes, each with its own `decision=` tag in the log:

| Outcome | `decision=` | Reply |
|---|---|---|
| The model chose `error_response` | `model_reject` | `{ok: 0, code, errmsg}` — the model's own |
| The model produced no response action | `fail_closed_no_answer` | `{ok: 0, code: 59, errmsg: "netget: no response produced for command '<name>'"}` |
| The model's answer would not encode | `fail_closed_unusable_answer` | `{ok: 0, code: 1, errmsg: "netget: request could not be processed"}` |
| The LLM call errored | `fail_closed_llm_error` | `{ok: 0, code: 365 or 1}` (see below) |

The LLM-error reply splits the two `WireFailure` categories onto distinct codes:
`Overloaded` → **365 `TemporarilyUnavailable`** with `errmsg: "netget: backend at
capacity, retry later"`, `Unavailable` → **1 `InternalError`** with `errmsg:
"netget: request could not be processed"`. The choice of 365 is constrained:
MongoDB's *driver-retryable* codes all describe replica-set failover
(`ShutdownInProgress`, `PrimarySteppedDown`, `NotWritablePrimary`) and claiming
one would send the driver hunting for a primary that does not exist. 365 says
"saturated, try again" without asserting anything about topology.

`{ok: 0}` is the only shape a driver reads as a command failure. Anything with
`ok: 1` is a *result*, and an empty `find` result means "no documents matched" —
a claim about the data that nothing here is in a position to make.

The connection stays open in all four cases; only `close_this_connection` and a
malformed wire message end it.

### Handshake

`hello` and `isMaster` are answered **in Rust**, by `hello_response()`, and never
reach the model. A driver refuses to use a server that does not advertise a wire
version range it supports, so those fields cannot be left to an instruction; this
matches how the sibling database protocols handle their handshakes (opensrv-mysql,
pgwire's startup handler, MSSQL's hand-written PRELOGIN/LOGIN).

Advertised: `minWireVersion` 0, `maxWireVersion` 17 (MongoDB 6.0),
`maxBsonObjectSize` 16 MiB, `maxMessageSizeBytes` 48 MB, `maxWriteBatchSize`
100000, `logicalSessionTimeoutMinutes` 30, `isWritablePrimary`/`ismaster` true.
No `saslSupportedMechs`, so drivers do not attempt authentication.

Everything else the driver sends (`ping`, `buildInfo`, `getParameter`,
`endSessions`, …) arrives as an ordinary `mongodb_command` event and must be
answered by the instruction or a handler.

Before this, `hello` was routed to the LLM, whose available actions produce only
`ok`/`n`/`cursor` shapes — no driver could complete a handshake, and all five
E2E tests failed at the connection step.

## Wire format

Request header (16 bytes, little-endian):
`messageLength | requestID | responseTo | opCode`

Request body for OP_MSG (2013): `flagBits (4) | sectionKind (1) | BSON document`.

Response: `messageLength | requestID=0 | responseTo=<requestID> | opCode=2013`,
then `flagBits=0 | sectionKind=0 | BSON document`.

### Input validation

`messageLength` is attacker-controlled and is range-checked to
`16..=MAX_MESSAGE_SIZE` (48 MB, the value real MongoDB advertises as
`maxMessageSizeBytes`) **before** it is used as an allocation size. A value below
16 used to underflow `(message_length - 16) as usize` into ~18 exabytes and abort
the process; `i32::MAX` allocated 2 GB per connection. Both were reachable with
16 unauthenticated bytes.

Anything other than opCode 2013 closes the connection rather than being skipped —
skipping left the client waiting forever for a reply it could parse. OP_QUERY
(2004), OP_COMPRESSED (2012) and section kind 1 are all rejected this way.

## Architecture

- `spawn_with_llm_actions` binds with `?` (so a bind failure surfaces as
  `ServerStatus::Error`) and registers the accept-loop `JoinHandle` via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- One task per connection. `handle_connection` wraps `run` so the connection is
  always marked `Closed` in `AppState` on exit.
- `tokio::io::split()` inside an inner scope so the stream is released before the
  disconnect event.
- No per-connection state machine: the read loop is sequential, so concurrent LLM
  calls on one connection cannot happen.

## Not implemented

- **Authentication** (SCRAM), **TLS**, **compression** (OP_COMPRESSED),
  **checksums**.
- **OP_MSG section kind 1** (document sequences) — a bulk insert that uses them
  is rejected.
- **Cursors** — `find_response` always returns `id: 0`, i.e. a single batch;
  `getMore` has no action.
- **Aggregation, transactions, change streams, GridFS, indexes, sharding,
  replica sets.**

## Testing

`tests/server/mongodb/e2e_test.rs`, declared in `tests/server/mod.rs`. Needs both
`mongodb-server` (the server) and `mongodb` (the client crate used by the test).
Five cases: find, insert, update, delete, error. All pass.

```bash
./cargo-isolated.sh test --no-default-features --features mongodb-server,mongodb \
    --test server -- --test-threads=100 mongodb
```

Header validation was checked by hand:

```bash
netget --mcp-http 18899 &
# start_server protocol=mongodb port=27117 event_handlers=[…]
# then send 16-byte headers with messageLength 0, i32::MIN, i32::MAX and opCode 2004
```

## Example prompts

```
Start a MongoDB server on port 27017 for database "shop" with a users
collection. Answer find with find_response, answer insert with insert_response,
and answer a query against an unknown collection with error_response code 26.
```

## References

- [MongoDB wire protocol](https://www.mongodb.com/docs/manual/reference/mongodb-wire-protocol/)
- [OP_MSG](https://github.com/mongodb/specifications/blob/master/source/message/OP_MSG.rst)
- [BSON specification](http://bsonspec.org/)
- [bson crate](https://docs.rs/bson/)
