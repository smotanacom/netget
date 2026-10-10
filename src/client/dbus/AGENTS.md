# D-Bus client

One connection to a message bus (or, with `bus: false`, a peer), over a Unix socket
(`socket_path`) or TCP (`remote_addr`). Uses the server's codec (`src/server/dbus/wire.rs`).

- **Authentication:** EXTERNAL as the process's uid, then ANONYMOUS if the server offers it
  (dbus-daemon on TCP cannot check EXTERNAL, so a TCP bus must `<allow_anonymous/>`).
- **Hello** after authentication unless `bus: false`; the unique name is in `dbus_connected`.
- **Calls** (`dbus_call`, and `dbus_request_name` / `dbus_add_match`, which are calls to the
  bus) are matched to their replies by serial: `dbus_reply` or `dbus_error_reply`. At most 64
  wait at once; one unanswered after `timeout_ms` (default 25 s, libdbus's) is reported as
  `org.freedesktop.DBus.Error.NoReply`. An injected call's `send_to_client` outcome carries the
  reply.
- **Incoming calls** (to a name NetGet owns) are `dbus_method_call`. The dispatcher binds the
  model's `dbus_return` / `dbus_error` to the call that raised the event and, exactly like the
  server, **fails closed**: a call expecting a reply that gets none is answered
  `org.freedesktop.DBus.Error.Failed`. Peer.Ping and GetMachineId are answered in Rust.
- **Signals** arrive as `dbus_signal` (the bus's own NameAcquired/NameLost are dropped; the
  RequestName reply already says it). `dbus_emit_signal` sends one.

Model turns run in a dispatcher task fed by a queue; turns follow one another at most
`MAX_FOLLOWUP_DEPTH` (8) deep, except that a call expecting a reply is always answered — past
the bound with the fail-closed error rather than silence.

Not implemented: Unix file descriptors, DBUS_COOKIE_SHA1, reading `DBUS_SESSION_BUS_ADDRESS`
(pass the socket explicitly).
