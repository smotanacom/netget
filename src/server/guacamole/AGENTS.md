# Guacamole server (in guacd's place)

The Apache Guacamole protocol on TCP 4822, the port guacd listens on. Guacamole clients reach
NetGet as they would reach guacd. NetGet is the remote desktop itself, not a gateway: it
connects to no VNC/RDP/SSH server. The model draws what the user sees.

## Framing (`wire.rs`, shared with the client)

- Instructions are `LEN.VALUE,…;` where **LEN counts Unicode code points, not bytes**. A
  byte count breaks on the first non-ASCII character.
- Bounds are guacd's own: `MAX_INSTRUCTION` 8192 bytes and `MAX_ELEMENTS` 128. They are
  checked while reading, so an oversized instruction closes the connection unanswered.

## Handshake (Rust)

1. `select`. Joining an existing connection (`$id`) is refused with an error.
2. `args`: `VERSION_1_3_0`, then the configured parameter names (startup param `parameters`,
   default `hostname, port, username, password`).
3. The client's `size`, `audio`, `video`, `image`, `timezone` and `name`, in any order, at
   most `MAX_HANDSHAKE_INSTRUCTIONS`.
4. `connect`. A client that answered the version gives it first; pyguacamole answers it
   empty, so the element count decides as well.

The whole handshake has a 15 s deadline.

## The model

`guacamole_connect {protocol, arguments, secret_arguments, width, height, image_types,
timezone}`:
- `arguments` leaves out any parameter whose name looks like a secret (password, passphrase,
  private-key, secret, token). Only their names are listed, in `secret_arguments`.
- The answer is `guacamole_accept` (plus drawing), or `guacamole_reject{message}`, which
  becomes `error,…,769` (CLIENT_UNAUTHORIZED).
- A failure or silence is `error,Internal error,512` (SERVER_ERROR) and the connection closes.

After `ready,$<uuid>` and `size,0,w,h`, the client's input becomes events:
- `guacamole_typed {text}`: printable keys are buffered (BackSpace edits) until Enter. One
  event per line, not per key.
- `guacamole_key {key, pending_text}`: other non-modifier keys (Escape, arrows, F-keys).
  Modifiers alone are ignored.
- `guacamole_click {x, y, button}`: a newly pressed button. Motion is not an event.
- `guacamole_clipboard_received {text}`: a clipboard stream, acked blob by blob, at most
  `MAX_CLIPBOARD` (64 KiB).

Drawing actions, each batch ended with a `sync`:
- `guacamole_fill{x?, y?, width?, height?, color}` becomes `rect` + `cfill` (14 = over).
- `guacamole_text{x, y, text, color, background, scale}` is rendered in Rust with font8x8 to
  a PNG and sent as an `img` stream, with ≤ 6048-character base64 blobs as guacd sends them.
- `guacamole_clipboard{text}` and `guacamole_disconnect{message?}`.

The dashboard's injected actions take the same path.

## The writer and other limits

A single writer task owns the socket and sends a `sync` every 5 s. guacamole-common's socket
has a 15 s read timeout, so a slow model must not starve it. A session idle for 300 s closes.

Not implemented: layers other than 0, audio, video, file transfer, joining, image formats
other than PNG.
