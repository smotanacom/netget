# VNC Protocol Implementation

**Status**: `DevelopmentState::Experimental` — LLM-authored, not human-reviewed against a
graphical viewer, but every declared event fires, every declared action is reachable, and the
E2E suite decodes real pixels off the wire.

RFB 3.8 (RFC 6143) over TCP, default port 5900 — not privileged, so `privilege_requirement`
is `None`.

## Hand-written RFB, not a crate

The `vnc` feature declares no dependencies beyond what `src/display/` already pulls in, and
that is deliberate. The *server* half of RFB 3.8 is a 12-byte version exchange, a one-byte
security negotiation, ClientInit/ServerInit, and seven fixed-length client messages. The Rust
VNC crates that exist are viewers (client side, with their own event loops) or bindings to
libvncserver, which would add a C system dependency to `dist` builds for maybe 200 lines of
framing. Hand-rolling is defensible here in a way it is not for, say, BGP: the state machine is
linear, every client message has a fixed length or a length prefix, and the whole thing is
covered by an RFB client written directly against the RFC in `tests/server/vnc/test.rs`.

## What the LLM controls

Rust owns the protocol, the framebuffer and the pixel encoding. The model owns **what is on
screen** and how the screen reacts to input. It never sees or produces pixels: it answers with
structured drawing commands, and `crate::display::DisplayCanvas` (tiny-skia + cosmic-text)
rasterises them.

### Events — all four fire

| Event | Raised when | Actions offered |
|---|---|---|
| `vnc_framebuffer_update_request` | a **non-incremental** FramebufferUpdateRequest arrives | render, no_change, disconnect |
| `vnc_key_event` | any KeyEvent, press or release | render, no_change, set_clipboard, disconnect |
| `vnc_pointer_event` | a PointerEvent whose **button mask changed** | render, no_change, disconnect |
| `vnc_client_cut_text` | ClientCutText arrives | set_clipboard, render, no_change, disconnect |

Two events are deliberately *not* raised on every matching message, because raising them would
spend one model round-trip per poll or per pixel of mouse travel:

- **Incremental update requests** are held open, which is what RFB is for: the request means
  "tell me when something changes", and the server may answer whenever it likes. A held request
  is answered by the next event that redraws. A viewer that has an idle screen therefore costs
  nothing.
- **Pointer movement with no button change** is dual-logged at DEBUG and dropped. A viewer emits
  a PointerEvent per pixel of travel.

Both are visible in the TUI/MCP status stream, so input is never silently invisible — the old
implementation logged PointerEvent at `trace!` only, which meant the only client input this
protocol receives did not appear where somebody watching a session would look.

### Actions

| Action | Effect |
|---|---|
| `vnc_render_display` | `commands` array → rasterised → sent as one Raw rectangle |
| `vnc_no_change` | screen left alone; an incremental request stays held |
| `vnc_set_clipboard` | RFB ServerCutText pushed to the client |
| `vnc_disconnect_client` | connection closed |

There are **no async (user-triggered) actions**. The server keeps no registry of live
connections, so an async action would have nothing to address; declaring one would be a promise
the code does not keep.

`commands` are flat, `type`-tagged objects — `background`, `clear`, `rectangle`, `text`, `line`,
`circle`, `window`, `button`, `textbox`, `ascii_art` — not the externally-tagged
`DisplayCommand` serde shape (`{"DrawText": {...}}`), which models produce badly.
`actions::parse_display_commands` translates, and rejects anything it does not understand rather
than silently dropping it: a screen half-drawn because one command was skipped is
indistinguishable from one the model meant.

Colours accept `#rgb`, `#rrggbb`, `#rrggbbaa`, 18 colour names, or `{"r":…,"g":…,"b":…,"a":…}`.

## Fail-closed behaviour

The three outcomes of an event are kept structurally distinct (`Decision` in `mod.rs`):

- **redraw** — the model decided what is on screen.
- **`vnc_no_change`** — the model explicitly decided to leave it alone. The previous screen
  stands, and a *full* request is still answered with it.
- **no usable answer** — an LLM error, a batch where every action failed, or an action list with
  nothing this protocol can execute. The client gets a **placeholder screen**
  (`PLACEHOLDER_BACKGROUND`, an on-screen diagnostic line) and a WARN on both the log and the
  status stream.

Silence is not an option on a full update request: RFB has no "no answer" reply, so a client
that asked for the whole screen and receives nothing waits forever. Equating "no answer" with
"nothing changed" would be exactly the fail-open shape that bit OAuth2.

## Failure behaviour

`VncConnection::consult` emits exactly one `decision=` line per event, so a backend outage, a
model that said nothing, and a model that explicitly refused are never the same log line. RFB
cannot carry the distinction — the placeholder screen looks the same whatever produced it — so
the log is the only place it can live.

| Outcome | On the wire | Log |
|---|---|---|
| `vnc_render_display` / `vnc_set_clipboard` / `vnc_no_change` | that screen or clipboard text; `vnc_no_change` re-sends the last screen on a full request | INFO `decision=model_answer` |
| `close_connection` | connection closed after the pending frame | INFO `decision=model_reject` |
| Model answered with nothing this protocol can use | placeholder screen, fixed caption | WARN `decision=model_silent` |
| The model's actions were refused by the executor, or its render result could not be decoded | placeholder screen, fixed caption | ERROR `decision=fail_closed_bad_action` |
| Backend failed | placeholder screen, fixed caption | ERROR `decision=fail_closed_llm_error` |
| Backend saturated (`WireFailure::Overloaded`) | placeholder screen, fixed caption | ERROR `decision=fail_closed_llm_overloaded` |
| Client chose a security type other than None | SecurityResult failure + a **byte-literal** reason | WARN `decision=protocol_error` |

`tests/server/vnc/test.rs::test_vnc_backend_failure_is_tagged_fail_closed` pins the backend
row; `test_vnc_placeholder_when_model_gives_no_usable_answer` pins the `model_silent` row and
asserts the backend row is *not* claimed in the same run.

**Nothing NetGet knows reaches the peer's viewer.** Two places could leak and neither does: the
placeholder's caption is one of four fixed strings chosen by the caller (`"the model gave no
screen to draw"` and friends), not `Decision::no_answer`, which holds the error text and is only
read as a flag; and the RFB SecurityResult reason — which a viewer renders verbatim in a dialog
box — is a `b"..."` literal. Keep both that way: this is precisely the class
`tests/wire_failure_test.rs` guards.

**Admission is not a model decision.** `perform_handshake` offers security type 1 (None) and
accepts any client that picks it, with no LLM call anywhere in the path — see "Limitations".
That is the documented design, not a fail-open introduced by a failure: there is no event on
which a model could ever have refused a viewer.

## Startup parameters

| Name | Type | Default | Notes |
|---|---|---|---|
| `width` | number | 800 | 16–4096, and `width * height` ≤ 3840×2160 |
| `height` | number | 600 | as above |
| `desktop_name` | string | `NetGet VNC` | shown in the viewer's title bar |

All three are read in `Server::spawn`; nothing is declared that is not read, and nothing is read
that is not declared. Validation happens before `add_server`, so a bad value is a clean error.

## Attacker-controlled input

Everything a client or a model can send is bounds-checked before it is allocated or indexed:

- `ClientCutText` length is capped at `MAX_CUT_TEXT_LEN` (1 MiB) **before** the buffer is
  allocated — the length is a client-controlled u32, so without the cap a nine-byte message asks
  for 4 GiB.
- The requested update region is ignored entirely; the announced framebuffer is always sent, so
  a client cannot ask for a 65535×65535 rectangle.
- An unknown client message type closes the connection. Its length is unknown, so the stream
  cannot be resynchronised; continuing would read the message body as message types.
- Display commands are capped at 256 per render, 4 levels of window nesting, 4096 bytes per
  string, 65535 per coordinate. No `unwrap()` on anything parsed from the network or the model.
- `ClientCutText` is decoded byte-per-character as Latin-1, which cannot fail, so malformed
  input cannot abort the connection task.
- Rendering runs on `spawn_blocking`, so a panic inside tiny-skia surfaces as an error on the
  connection instead of silently killing the task while the server still reports `Running`.

## Wire details

- **Security**: type `None` (1) only. Any client that connects is admitted; there is no VNC-Auth
  and no password. `update_vnc_connection_auth(..., true, None)` records "authenticated", it
  does not decide it. A client that picks any other type gets a SecurityResult failure *with*
  the RFB 3.8 reason string.
- **Encoding**: Raw only, one rectangle covering the whole framebuffer, 32bpp BGRX matching
  ServerInit. Every update is `width * height * 4` bytes uncompressed.
- **Frames are serialised into one `Vec<u8>` and written with a single `write_all`.** This used
  to be one awaited `write_u8` per byte — 1.9 million awaited syscalls per 800×600 frame, all
  while holding the write lock.
- The accept loop breaks on error rather than spinning, and its `JoinHandle` is registered via
  `AppState::register_server_task()` so `stop_server` releases the socket.
- A connection is removed from the server view on every exit path, including a failed handshake.

## Dashboard injection (`[ disconnect this peer ]`, live counters)

Every connection registers a peer handle (`server::peer_support`) right after it is tracked and
before the handshake, and removes it on every exit path (EOF, read error, unknown message,
injected disconnect) through the single cleanup in `handle_connection`. `AppState::send_to_peer`
runs the injected action through the same executor the LLM path uses.

- **`[ disconnect this peer ]`** injects `{"type":"close_connection"}`. `execute_action` maps
  both `close_connection` and `vnc_disconnect_client` to `ActionResult::CloseConnection`, which
  the generic peer task half-closes; the message loop then reads EOF and runs its teardown.
- **The drawing verbs are a Custom-result gap, and deliberately stay one.** `vnc_render_display`
  and `vnc_set_clipboard` return `ActionResult::Custom`, and the generic peer task cannot
  execute them — rendering needs this connection's framebuffer state (`last_frame`, `dirty`,
  `pending_request`, negotiated width/height), which is owned by the message loop, so an
  out-of-loop render would race it. The peer task reports these as "executed" without touching
  the wire. Only disconnect is wired; "message this peer" with a draw is deliberately not.
  The redis-style fix (encode in `execute_action`, return `Output`) cannot work here: a
  FramebufferUpdate is not a pure function of the action — whether it may be written at all
  depends on `pending_request` (RFB lets the server answer only an outstanding update
  request), and `vnc_no_change` semantics depend on `last_frame`. Closing the gap would take a
  bespoke per-connection peer command task in the BGP style (`bgp/mod.rs`'s own
  `spawn_peer_command_task`): hoist the framebuffer state out of `VncConnection` into a shared
  handle (`Arc<Mutex<..>>`) used by both the message loop and the peer task, and rasterise
  off-loop via the same `spawn_blocking` path. That is a restructuring of the message loop,
  not a contained encoder move, and has not been done.
- **Live counters.** `update_connection_stats` is called on every client message read (in the
  message loop) and every frame / ServerCutText write (`send_pending_frame`,
  `send_server_cut_text`), so the rail's `↓ ↑` counters and `last_activity` move. Handshake and
  ServerInit bytes, written before the loop by static helpers without the connection context,
  are not counted.

Test: `tests/server/vnc/peer_inject_test.rs` (zero LLM calls).

## Limitations

- No VNC-Auth (type 2), no TLS, no VeNCrypt.
- `SetPixelFormat` is parsed and ignored — a client that negotiates 8- or 16-bit colour renders
  garbage.
- `SetEncodings` is parsed and ignored — no CopyRect, RRE, Hextile, ZRLE or Tight, and no
  compression.
- No `DesktopSize` pseudo-encoding: the framebuffer size is fixed at startup.
- Updates are always full-screen; there is no damage tracking, so `vnc_render_display` costs a
  whole frame on the wire even for a one-pixel change.
- One connection is handled strictly sequentially — a model call for one message completes
  before the next is read. RFB messages are small and the socket buffers them, but input that
  arrives during a call is delayed by it.
- Each connection has its own framebuffer; the ClientInit shared flag is read and ignored.
- Two model round-trips per keystroke (press and release) unless a script or static handler is
  used. Prefer a script handler for anything deterministic.

## Manual verification

```bash
./cargo-isolated.sh run --no-default-features --features vnc --release
# then: "listen on port 5900 via vnc showing a dark blue desktop with the text Hello"
vncviewer localhost:5900     # or TigerVNC / RealVNC
```

Expect an 800×600 window whose contents are whatever the model drew, a placeholder screen
reading `NetGet VNC: …` if the model failed to answer, and `VNC KeyEvent:` /
`VNC PointerEvent:` lines in the status panel. macOS Screen Sharing negotiates VNC-Auth and
will not connect.

## References

- [RFC 6143 — The Remote Framebuffer Protocol](https://tools.ietf.org/html/rfc6143)
- [rfbproto](https://github.com/rfbproto/rfbproto)
