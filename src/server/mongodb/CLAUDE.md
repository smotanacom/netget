# MongoDB Server Implementation

MongoDB wire protocol server. There is no Rust MongoDB *server* library, so
OP_MSG is parsed and built by hand; `bson` v3.0 only does document
encoding/decoding. **There is no storage** — no collections, no documents, no
in-memory map. The LLM answers every command except the `hello`/`isMaster`
handshake, which the server answers itself.

**State**: `Beta`, and `metadata()` agrees — this line used to say `Experimental`
while the code claimed `Beta`, so neither was checkable. The evidence is
`tests/server/mongodb/e2e_test.rs`: five cases driven by the **official `mongodb`
Rust driver**, not `#[ignore]`d and with no skip-when-missing gate, covering the
handshake plus find/insert/update/delete/error. Not Stable — spec compliance and
scripting support have not been reviewed.
**Port**: 27017 by default. **Privilege**: `None` (27017 > 1024).
**Startup parameters**: none. `send_first` was declared and discarded; MongoDB is
client-first, so it could never have been honoured.
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
| `reason` | `client_disconnect`, `close_this_connection`, `idle_timeout`, `invalid_message_length`, `incomplete_message_body`, `unsupported_opcode`, `malformed_op_msg` |

The write half is shut down and the connection marked closed before this event
fires, so the LLM round-trip does not hold the connection open. A session that
ended in a read or write **error** raises no disconnect event at all — the
reasons above describe how a session ended, and there is none to report.

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

Every count parameter is declared `required: true` and the executor **enforces
it** rather than substituting a default. Each one is an assertion about what
happened to the data: a defaulted `inserted_count` acknowledged a write nothing
said had occurred, and a defaulted `matched_count` of 0 says "your filter matched
nothing", which is a claim about a collection this server does not have.
`error_response` is the same shape — code 0 is MongoDB's `OK`, so defaulting it
produced a failure naming success as its cause. A missing field is reported to the
model for repair; if repair fails, the `fail_closed_no_answer` path below answers
`{ok: 0}`.

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

A fifth refusal happens before the model is involved at all: a command document
nested deeper than 64 levels is answered `{ok: 0, code: 15, errmsg: "BSONObj
exceeded maximum nested object depth"}` — MongoDB's own `Overflow` code and
wording, a constant — logged `decision=fail_closed_bson_too_deep`, and the
connection stays open, because the message was read whole. See [BSON
nesting](#bson-nesting).

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

`messageLength` bounds one buffer; `BODY_READ_TIMEOUT` (30s) bounds how long the
peer may take to deliver it. Sixteen bytes claiming a 48 MB body used to pin 48 MB
per connection for as long as the peer cared to stay silent, so a hundred idle
connections was 4.8 GB bought with 1.6 kB of traffic. The rest of a message whose
header has already arrived is in flight by definition, so the deadline refuses a
stalled peer without truncating a legitimate one; the connection ends with
`reason: incomplete_message_body`.

The *header* read is bounded separately — see [Connection bounds](#connection-bounds).

### BSON nesting

`bson` 3.0 decodes with no depth limit: `Document::from_reader` converts through
`TryFrom<&RawDocument> for Document` → `TryFrom<RawBsonRef> for Bson` →
`TryFrom<RawBson> for Bson`, which calls back into the first for every embedded
document, array and code-with-scope scope. An embedded document costs the peer
seven bytes a level, and measured against 3.0.0 a **debug** build overflows a
2 MiB stack at **284 levels** (2 277 bytes), a release build at 1 220. The first
`OP_MSG` of an unauthenticated connection went straight to it, so ten kilobytes
killed the whole process — a `SIGSEGV` on the guard page, not a panic.

`parse_op_msg` now runs `crate::utils::bson_depth::scan_bson_document` on the
kind-0 section first: an iterative walk, the end offset of each open document in
a fixed 64-slot array, every element sized exactly as `bson`'s own
`RawIter::get_next_kvp` sizes it, every length checked against the document that
contains it. `Complete` is decoded (and only the bytes the scan measured, so
`bson`'s `reader_to_vec` never reserves `Vec::with_capacity` on a declared length
the message does not back — it would otherwise reserve 2 GiB for a 26-byte
message); `TooDeep` gets the reply above; `Malformed` closes the connection with
`reason: malformed_op_msg`, as an undecodable document always has.

`MAX_BSON_DEPTH` is **64**, not MongoDB's 100-level limit for stored documents.
A command wraps a document in two more levels, and at ~7.4 KB of stack per level
in a debug build 102 levels spends ~750 KB of a 2 MiB worker on the decode alone,
before this server walks the same document recursively three more times
(relaxed extended JSON for the event, `Debug` in a `trace!`, `Drop`). The cost is
that a document nested 63 to 100 deep, which real MongoDB would store, is
refused; no driver-generated command comes near it.

## Connection bounds

| Bound | Value | Applies to | Why this number |
|---|---|---|---|
| `FIRST_HEADER_READ_TIMEOUT` | 30s | the 16-byte header, before this session has answered anything | MongoDB is client-speaks-first: the server says nothing until an OP_MSG arrives, and every real driver opens with `hello`/`isMaster` inside its own connect path. A peer that has connected and sent no header has started nothing. |
| `IDLE_BETWEEN_MESSAGES_TIMEOUT` | 600s | the 16-byte header, once a message has been answered | A driver's pooled connection is legitimately idle for minutes between operations; `heartbeatFrequencyMS` defaults to 10 s, so a monitoring connection stays far inside this. Forever is not a legitimate configuration. |
| `BODY_READ_TIMEOUT` | 30s | the announced body, once its header has arrived | The rest of a message whose header arrived is in flight by definition. |
| `MAX_MESSAGE_SIZE` | 48 MB | the announced `messageLength` | The value this server advertises as `maxMessageSizeBytes`. |

**NetGet's own MongoDB client is *lazy*, so it is never the silent peer this bound closes:**
`MongoClient::with_options` does no I/O at the call site, and the socket the driver's SDAM
monitor does open sends `hello` on its own initiative within milliseconds —
`PROTOCOL_QUALITY.md`'s three-state test.

**The header pair is what makes `[ disconnect this peer ]` take effect.** The
dashboard's disconnect half-closes the write side, which a peer that is not
reading never notices, so without a deadline on the header read the connection
task stayed parked in `read_exact` until the peer itself closed — holding the
socket, the registered task and the `AppState` row behind an operator action that
reported success. `BODY_READ_TIMEOUT` never covered it: that one arms only once
sixteen bytes have already arrived, so a peer sending zero bytes, or eight, was
outside every bound in the file.

**Two numbers, not one, for the reason `whois` states**: "has said nothing at all"
and "has gone quiet mid-session" are different claims. Collapsing them onto the
short bound would close a pooled driver connection between operations.

**Both are armed lazily, per read.** The `tokio::time::timeout` future is built at
the top of the loop, *after* the previous message's LLM round-trip — or a `manual`
rule parked for a human, 300 s by default — has finished, so no clock runs during
that work and a long park cannot evict a live session. That is the TFTP eviction
defect the project `CLAUDE.md` records, stated in reverse: what is bounded is a
peer holding a connection while saying nothing, never a server taking its time to
answer. The `reason` reported on the disconnect event is `idle_timeout`.

`tests/server/mongodb/connection_bounds_test.rs` drives all of it from a raw
socket: a peer that says nothing, a peer that sends half a header, and — the
control that stops the pair collapsing back into one number — a peer that
completes the `hello` handshake and then goes quiet past the 30 s bound and
is *not* closed.

Anything other than opCode 2013 closes the connection rather than being skipped —
skipping left the client waiting forever for a reply it could parse. OP_QUERY
(2004), OP_COMPRESSED (2012) and section kind 1 are all rejected this way.

## Architecture

- `spawn_with_llm_actions` binds with `?` (so a bind failure surfaces as
  `ServerStatus::Error`) and registers the accept-loop `JoinHandle` via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- One task per connection. `handle_connection` wraps `run_session` so the peer
  handle is released, the write half shut down and the connection marked `Closed`
  in `AppState` on **every** exit, including the error paths — nothing `?`s past
  the teardown.
- `tokio::io::split()` (owning, never a clone) in `handle_connection`, so the
  write half can be shared with the peer-command task; the read half is dropped
  when `run_session` returns, before the disconnect event.
- No per-connection state machine: the read loop is sequential, so concurrent LLM
  calls on one connection cannot happen.
- `update_connection_stats` fires on every message read and every reply written
  (`record_received` / `write_response`), so the rail's `↓ ↑` counters and
  `last_activity` reflect the socket. MongoDB is connection-oriented and does
  **not** declare `.connectionless()`, so the 10-second idle sweep leaves its
  connections alone.

### Dashboard injection (peer handle)

Every connection registers a peer handle (`server::peer_support`) **before its
first read**. MongoDB is client-speaks-first, so this server says nothing until a
command arrives, and a `manual` rule parks that very first command — the operator
must be able to reach, or hang up, a connection that is waiting on their own
answer. The write half is an `Arc<Mutex<WriteHalf>>` shared by the session loop
and the peer-command task, so an injected write can never land inside an OP_MSG
the loop is emitting.

**What an injected action can and cannot do, exactly:**

| Injected | Result |
|---|---|
| `close_connection` (what `[ disconnect this peer ]` sends) | half-close; the peer reads EOF, the connection is marked closed and the handle removed |
| `close_this_connection` | the same; it is the name the *model* uses |
| `find_response` / `insert_response` / `update_response` / `delete_response` / `error_response` | `ClientSendOutcome::Executed` — **executed, but nothing reaches the wire** |

The five wire verbs are `ActionResult::Custom`, not `ActionResult::Output`:
`peer_support` writes `Output` bytes and reports everything else as executed, and
the OP_MSG framing for these lives in the read loop, which is the only thing that
holds the request's `requestID` (the reply's `responseTo`) and the `$db` +
collection the namespace is built from. An OP_MSG that answers no request has no
`responseTo` to carry, so there is nothing an injected one could legally emit.
`[ message this peer ]` is therefore of limited use on MongoDB and this is
deliberate honesty, not a gap to be papered over — the same situation as
`src/server/db2/`.

`close_connection` is accepted by `execute_action` as an unadvertised alias of
`close_this_connection` (it is in neither `get_sync_actions()` nor any event's
action list, so the model's tool list is unchanged). Without it the dashboard's
`[ disconnect this peer ]`, which injects a bare `{"type": "close_connection"}`,
would fail as an unknown action.

`tests/server/mongodb/peer_inject_test.rs` proves all of this with zero LLM calls.

**The gap this section used to name is closed.** The header read had no deadline
— only the *body* was bounded, by `BODY_READ_TIMEOUT` — so after an injected
half-close the connection was marked closed and the handle gone immediately while
the task itself stayed parked in `read_exact` until the peer closed its own side.
`[ disconnect this peer ]` reported a hang-up that had not happened. See
[Connection bounds](#connection-bounds): the header read is bounded now, by the
same first/idle pair `mssql`, `db2` and `whois` use.

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

`peer_inject_test.rs` needs only `mongodb-server` — it speaks raw OP_MSG rather
than driving the client crate, so it is gated on the server feature alone.

`bson_depth_test.rs`, also raw OP_MSG and server-only: a 10 000-level command
gets the `Overflow` reply and both the same connection and a fresh one still
answer `hello`; a command exactly 64 documents deep is answered and 65 is
refused; a document declaring `i32::MAX` bytes closes the connection. Without
the scan the test binary aborts with `stack overflow`.
`fuzz/fuzz_targets/bson_document.rs` drives the scan and `bson` as a pair, with
a depth-bomb seed.

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
