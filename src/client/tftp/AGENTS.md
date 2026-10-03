# TFTP Client (RFC 1350)

LLM-driven file transfer over UDP. NetGet owns the socket and the packet framing; the model
chooses the operation, acknowledges every inbound block, and supplies every outbound block.

## History: why this was disabled for nine months

Commented out of `client_registry.rs` and `src/client/mod.rs` on 2025-11-20 by `66212c37`
("Client needs `call_llm_for_client` API updates (deferred to future commit)"). The future
commit never came. Re-enabled 2026-08 by porting it to the current signature.

The disabling reason was real at the time and is now stale: `call_llm_for_client` takes eight
arguments (`llm_client, state, client_id, instruction, memory, event, protocol, status_tx`)
and lives in `crate::client::llm_budget`. The old code called a three-argument version in a
module that no longer exists. `src/client/rip/` is the working reference for the current
shape.

## The bug re-enabling it exposed

`call_llm_for_client` (`src/llm/action_helper.rs`) builds the model's tool list from
**`get_async_actions()` alone**. It never reads `get_sync_actions()`, and — unlike the server
path — it never reads `event.event_type.actions`.

So an action declared only as *sync* is offered to the model by nothing, and if the model
names it anyway (because the event's example shows it), the executor rejects it as
`Unknown Action`. `send_ack` was in exactly that position: every DATA block came back with an
unknown-action error and the transfer stalled at block 1.

The fix is that `get_async_actions()` returns **every** action, sync ones included. The RIP
client already did this; it reads like duplication and is load-bearing. This is the
client-side twin of the "declared but unreachable" server defect described in the root
`CLAUDE.md`, and unlike the server case there is no test guarding it across all clients —
`tests/event_action_declarations_test.rs` walks the *server* registry only.

## Flow

```
open_client  ──> bind UDP 0.0.0.0:0
             ──> event tftp_connected
                  model answers tftp_read_file / tftp_write_file / disconnect
                  (nothing else starts a transfer — the client never invents a request)

read:   RRQ ──> DATA n ──> event tftp_data_received ──> model: send_ack n ──> ...
        final block (< 512 bytes) ──> event tftp_transfer_complete

write:  WRQ ──> ACK 0 ──> event tftp_ack_received ──> model: send_data_block 1 ──> ACK 1 ──> ...
        a block shorter than 512 bytes ends the transfer
```

**Server TID.** RFC 1350 §4: the server answers from a freshly allocated port, and every
subsequent client packet must go *there*, not to port 69. `run_transfer` learns the address
from the first reply. A client that keeps writing to the well-known port appears to work and
then hangs after block 1, which is why `tests/client/tftp/e2e_test.rs` uses a two-socket
server.

## Events and actions

| Event | Raised when | Actions offered |
|---|---|---|
| `tftp_connected` | socket bound | `tftp_read_file`, `tftp_write_file`, `disconnect` |
| `tftp_data_received` | DATA arrives (read) | `send_ack`, `disconnect` |
| `tftp_ack_received` | ACK arrives (write) | `send_data_block`, `disconnect` |
| `tftp_transfer_complete` | final block seen | `disconnect` |
| `tftp_error` | ERROR packet arrives | `disconnect` |

All five are emitted. `tftp_write_file` deliberately carries **no file content**: the WRQ is
sent, the server answers ACK 0, and the model supplies each block in response to an
`tftp_ack_received` event. That is symmetric with the read path and keeps the model in charge
of what goes on the wire — at the cost of one LLM call per 512 bytes, which is stated in
`metadata().notes`.

## Encoding

`data_hex` — inbound and outbound. The name states the encoding and the executor really
performs `hex::decode` (`send_data_block`), so the round trip is symmetric. This is the shape
the `send_tcp_data` fix (`d70bb5b5`) established; TFTP payloads are binary by definition, so
there is no useful "utf8" alternative to offer.

## Limitations

- **No retransmission.** A 5-second timeout aborts the transfer. RFC 1350's timeout/retry is
  not implemented.
- **No RFC 2347 options** — no `blksize`, `timeout` or `tsize` negotiation. Block size is
  always 512.
- **`netascii` is carried, not applied.** The mode string reaches the wire but the payload is
  passed through unmodified; no CR/LF translation happens.
- **One transfer per client.** The first `tftp_read_file`/`tftp_write_file` starts it; there
  is no way to begin a second on the same client.
- **Write transfers are untested.** The code path exists and the events fire, but
  `tests/client/tftp/` covers reads only.

## Example

```
connect to 192.168.1.1:69 via tftp. Read file pxelinux.0 in octet mode and report its size.
```

## Command channel (injected actions)

The client registers a `command_support` command channel, so `AppState::send_to_client` — the
dashboard's `[ send ]` — can execute an action inside the running client. Registration happens
**before** the `tftp_connected` LLM call, which matters more here than elsewhere: that call
runs *inline* in `connect()`, so a `*` → manual routing rule parks client creation itself, and
`[ send ]` has to work for the whole park.

Every verb reaches the wire, so every successful outcome is a real byte count:

| Action | `ClientSendOutcome` |
|---|---|
| `send_ack` / `send_data_block` | `Sent { bytes_sent }` — one `send_to` of the packet the protocol's own executor built |
| `tftp_read_file` / `tftp_write_file` | `Sent { bytes_sent }` — the RRQ/WRQ send is **awaited** before the transfer loop is spawned, so this is a real count and not a dispatch |
| `disconnect` | `Disconnected`; the command loop ends and the handle is dropped |
| bad or unknown parameters | `Rejected { error }` |

**The destination is the current transfer address, not the well-known port.** `transfer_addr`
moved from a local in `run_transfer` to a shared `Arc<Mutex<SocketAddr>>` for exactly this
reason: the receive loop rewrites it to the server's TID after the first reply (RFC 1350 §4),
and an injected ACK sent to port 69 after that point would be ignored by a conforming server.

This also relaxes the "one transfer per client" limitation for the injected path: an injected
`tftp_read_file` resets `transfer_addr` and starts a fresh transfer on the same socket.
