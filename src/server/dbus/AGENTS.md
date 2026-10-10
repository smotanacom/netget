# D-Bus server

NetGet as a small message bus that every call ends at: TCP always (the instance's address) and
a Unix socket when `socket_path` is given. Hand-rolled wire protocol in `wire.rs`, shared with
the client (`src/client/dbus/`). Clients: libdbus (`dbus-send`), GDBus (`gdbus`), dbus-next,
anything that speaks D-Bus 1.

## Authentication

SASL, line by line, before any message (`authenticate`): EXTERNAL is offered only on the Unix
socket and succeeds only for the uid `SO_PEERCRED` reports (an empty identity means "the
credentials you see"); ANONYMOUS when `allow_anonymous` (default true) — the only mechanism a
TCP peer can complete, since a TCP uid cannot be checked. NEGOTIATE_UNIX_FD is answered ERROR.
No DBUS_COOKIE_SHA1. Lines are capped at 16 KiB, the handshake at 32 lines and 10 s.

## What is answered in Rust, and what the model answers

The bus's own methods are deterministic: Hello (unique name `:1.<connection id>`, followed by
NameAcquired), RequestName (1 primary owner / 3 exists / 4 already owner, NameAcquired on 1),
ReleaseName, GetId, ListNames, ListActivatableNames, NameHasOwner, GetNameOwner,
AddMatch/RemoveMatch (accepted, not filtered: each connection talks only to NetGet), and
`org.freedesktop.DBus.Peer` Ping/GetMachineId. Names are shared across the server's
connections and released when the owner disconnects.

Every other METHOD_CALL is `dbus_method_call` for the model (path, interface, member, sender,
destination, signature, args as JSON, no_reply_expected); a SIGNAL is `dbus_signal_received`.
The model answers with `dbus_return` (signature + values), `dbus_error` (name + message), and
optionally `dbus_emit_signal`. `DbusProtocol::for_call` carries the call and the connection's
serial counter, so the answer is bound to its call and every message gets its own serial. An
injected `dbus_emit_signal` (the dashboard's "message this peer") uses serials from
0x80000000 up.

**Fails closed.** A call expecting a reply that the model leaves unanswered gets
`org.freedesktop.DBus.Error.Failed` "No answer was produced for this call"
(`decision=fail_closed_no_answer`); a backend failure gets `...Error.LimitsExceeded` when
overloaded, `...Error.Failed` otherwise, with only the category text on the wire.

## Values

JSON ↔ D-Bus by signature (`wire::marshal` / `R::get`): numbers range-checked per type,
`a…` arrays, `a{…}` objects (or `[key, value]` pairs), structs as arrays, variants as
`{"signature": "...", "value": ...}` or a plain value with an inferred signature. NetGet writes
little-endian and reads both byte orders.

## Bounds

- A message is at most 1 MiB (`MAX_MESSAGE_BYTES`), checked from the 16-byte fixed header
  before the rest is read.
- Values nest at most 32 deep, **variants included** (`MAX_DEPTH`). The specification allows
  64, but a decoded value becomes JSON inside an event and NetGet's event/handler pipeline
  bounds JSON at 64 levels: a 64-deep value was refused there, as a fail-closed error, instead
  of here. The variant counter is what matters — a signature is at most 255 bytes, but each
  variant carries its own signature in the data.
- Idle authenticated connections close after `idle_timeout_secs` (default 3600).

Not implemented: Unix file descriptors, DBUS_COOKIE_SHA1, routing between connections
(signals and calls go to NetGet only), introspection unless the model answers Introspect.
