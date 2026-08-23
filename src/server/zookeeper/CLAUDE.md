# ZooKeeper Server Implementation

## Status: Experimental

Real ZooKeeper clients complete a session against this server, read data and children, and
receive error codes. It was `Incomplete` because they could not: `parse_request` read bytes
0..4 as the xid and 4..8 as the opcode, but a `ConnectRequest` carries **neither**, so a connect
was reported to the handler as `operation: "unknown"` and the `ConnectResponse` the client
blocks on was never produced. Nothing downstream of the handshake had ever run.

## Session lifecycle

The first frame on a connection is the handshake, and it has a different shape from every
frame after it:

```
ConnectRequest   [4 len][4 protocolVersion][8 lastZxidSeen][4 timeOut][8 sessionId][4 16][16 passwd][1 readOnly?]
ConnectResponse  [4 len][4 protocolVersion][4 timeOut][8 sessionId][4 16][16 passwd][1 readOnly?]
Request          [4 len][4 xid][4 opcode][4 pathLen][pathLen path]... (rest not decoded)
Reply            [4 len][4 xid][8 zxid][4 err][body]
```

`parse_connect_request` validates strictly — `protocolVersion` must be 0 and the password
buffer must be exactly 16 bytes — and a first frame that fails those checks closes the
connection instead of being parsed as a request header. That strictness is the regression
guard: the permissive reading is precisely the bug that was fixed.

The `readOnly` byte is optional (pre-3.4 clients omit it). Whether the request carried it
decides whether the reply carries it.

### Negotiation

- **Timeout** is clamped into `4000..=40000` ms — ZooKeeper's own `2 * tickTime` and
  `20 * tickTime` with the default `tickTime` of 2000. A non-positive request gets the minimum.
- **Session id** is minted per connection (never 0, which means "no session"), or, if the
  client presented one, echoed back together with the password it presented. There is no
  session table to check it against (see *No storage*), so a reconnect resumes rather than
  being told its session expired — answering `timeout: 0` would be a lie about state that was
  never kept.
- **`readOnly`** is always false: read-only mode means "serve stale reads while partitioned
  from the quorum", and there is no quorum.

### What the server answers itself, and why

The handshake, `ping` (opcode 11) and `closeSession` (opcode -11) never reach the LLM.

They carry no content decision, and routing them through the model would make an outage or a
refusal indistinguishable from a successful session — the fail-open shape called out as the
most dangerous pattern in the root `CLAUDE.md`. A ping also arrives every `timeout / 3` on an
idle connection, so an LLM round-trip per ping would be the cost of doing nothing.

Everything a handler can actually decide still goes through `call_llm`.

## Event

### `zookeeper_request`

| Field | Type | Meaning |
|---|---|---|
| `xid` | integer | Request transaction id. **Echo this back.** |
| `operation` | string | Opcode name, or `"unknown"` |
| `op_code` | integer | Numeric opcode |
| `path` | string | Leading path string, or `""` when absent/undecodable |

Opcodes named: 1 create, 2 delete, 3 exists, 4 getData, 5 setData, 6 getACL, 7 setACL,
8 getChildren, 9 sync, 11 ping, 12 getChildren2, 13 check, 14 multi, -11 closeSession.

## Actions

Jute encoding is done **in Rust**. The model picks an action and supplies plain fields; it
never writes bytes. Which action answers which operation:

| Operation | Action | Body it encodes |
|---|---|---|
| `getData` | `zookeeper_data` | `buffer(data)` + `Stat` |
| `getChildren` / `getChildren2` | `zookeeper_children` | `vector<string>` (+ `Stat` if `include_stat`) |
| `exists`, `setData` | `zookeeper_stat` | `Stat` |
| `create` | `zookeeper_created` | `buffer(path)` |
| anything else, and every error | `zookeeper_response` | nothing, or `data_hex` |

All five take the same header fields: `xid` (optional), `zxid` (required), `error_code`
(optional, default 0). A non-zero `error_code` sends a header-only reply on every action —
real ZooKeeper sends no body with an error, and appending one desynchronizes the client.

`Stat` is the 68-byte Jute struct; the action supplies `zxid`, `version`, `data_length` and
`num_children` and the rest is filled consistently.

### Correlation

A client matches replies to requests by xid alone. Two things guarantee it:

- `xid` is in the event data, so a script handler can read it and a static handler can
  interpolate it — `"xid": "{{event.xid}}"`.
- If the action omits `xid`, the connection loop substitutes the xid of the request being
  answered. It never defaults to 0; xid 0 is not a valid reply to a real request (negative
  xids are reserved for pings and watch notifications) and leaves the client waiting forever.

## No storage

No znode tree, no session table, no data of any kind. Every reply comes from the handler.

## When netget itself cannot answer

ZooKeeper is strictly request/response and correlates by xid alone, so writing nothing leaves
the client blocked until its own session timeout. Every request that reaches a handler is
therefore answered, including when the handler fails:

| Situation | Reply | Log |
|---|---|---|
| handler answered with a non-zero `error_code` | that code | `decision=model_reject` |
| handler ran but produced no `zookeeper_response` | `ZSYSTEMERROR` (-1) | `decision=fail_closed_no_answer` |
| LLM call errored, `WireFailure::Unavailable` | `ZSYSTEMERROR` (-1) | `decision=fail_closed_llm_error` |
| LLM call errored, `WireFailure::Overloaded` | `ZOPERATIONTIMEOUT` (-7) | `decision=fail_closed_overloaded` |

The two failure codes differ on purpose: `ZOPERATIONTIMEOUT` reads as transient so a client
backs off and retries, `ZSYSTEMERROR` as a server-side fault. The full error goes to
`tracing::error!` and the status stream and **never** to the wire — a ZooKeeper error frame is
header-only, so there is no free-text field to leak a backend URL, model name or `anyhow` chain
into. Keep it that way: do not add a body to these replies.

## Remaining limitations

1. **Only the request header and the leading path are decoded.** The watch flag, znode data on
   `create`/`setData`, ACLs and version numbers are read off the wire and discarded, so a
   handler cannot see what the client wanted to write.
2. **No watches.** They need a second, server-initiated frame path (xid -1).
3. **No session ever expires**, because no session is ever tracked.
4. **`data_hex` still exists** on `zookeeper_response` as an escape hatch for opcodes with no
   structured action. It asks for hand-written Jute, which models do not produce reliably;
   prefer adding a structured action over using it.

## Robustness

- **Remote panic (fixed).** `parse_request` cast the path length to `usize` before checking it.
  A length of `-1` became `usize::MAX`, `12 + usize::MAX` wrapped to `11`, the guard
  `payload.len() >= 11` passed, and `&payload[12..11]` panicked with start > end. The length is
  now validated as `i32` against the bytes actually remaining, before any widening.
- **Frame length.** The 4-byte prefix is validated as `i32` in `8..=1048576` (ZooKeeper's own
  `jute.maxbuffer` default) before being widened.
- **Accept loop.** A persistent accept error (EMFILE) breaks the loop instead of spinning and
  flooding the unbounded status channel.
- **Connection tracking.** Connections are registered with `add_connection_to_server`,
  byte/packet counters are updated on every frame read and written (`update_connection_stats`,
  length prefix included), and the connection is marked closed on exit.

## Dashboard injection (peer handle)

Every connection registers a peer handle (`peer_support::register_peer_channel`) before its
first frame is read and removes it on every exit path, so the rail offers
`[ message this peer ]` / `[ disconnect this peer ]` even while a request is parked on a
manual rule.

The wire verbs all return `ActionResult::Custom { name: "zookeeper_response" }` — framing is
done in `mod.rs`, not in the action — so the generic `spawn_peer_command_task` would report
them as "executed" without writing. `ZookeeperServer::spawn_peer_command_task` is therefore a
protocol-owned copy of that task whose only addition is a Custom arm that frames the result
through `custom_reply_body`, the same function the LLM path uses, and writes it to the shared
`Arc<Mutex<WriteHalf>>`. An injected reply that names no `xid` goes out with xid **-1** (the
watch-notification xid, the only frame a real server originates) rather than claiming to answer
a request the operator cannot see. `close_connection` has an explicit arm in `execute_action`
(not offered to the model) so "disconnect this peer" half-closes the socket.

`tests/server/zookeeper/peer_inject_test.rs` covers all of it with zero LLM calls.
- **Invalid `data_hex`** is a hard error, not a silently body-less reply.

The accept-loop `JoinHandle` is registered via `AppState::register_server_task()`, so
`stop_server` releases the port. Per-connection tasks are still untracked — a project-wide gap.

## Testing

`tests/server/zookeeper/e2e_test.rs` drives the real `zookeeper-async` client plus one
byte-level handshake test. See `tests/server/zookeeper/CLAUDE.md`.
