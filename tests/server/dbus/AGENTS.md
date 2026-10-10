# D-Bus server tests

No LLM calls. `POLICY` (a Python script handler) answers Ping with `pong:<arg>`, Sum with the
sum of an int array, Echo with its own arguments and signature, Notify with an empty return
plus a `Changed` signal, Deny with `net.netget.Error.Denied`, and anything else with nothing
(the fail-closed path).

- `real_client_test.rs` — three independent clients, each decoding NetGet's bytes itself:
  **dbus-send** peer-to-peer over TCP (ANONYMOUS): Ping, Sum, Echo of a dict, a variant, an
  object path, a uint64 at its maximum and a boolean, Deny, and Silent answered
  `org.freedesktop.DBus.Error.Failed`; **gdbus** over the Unix socket with ANONYMOUS off
  (EXTERNAL against the peer credentials), Hello, Ping, the bus's ListNames, Deny; **python
  dbus-next**: Hello, RequestName answered 1, Notify's return and its `Changed` signal.
- `wire_test.rs` — raw sockets with hand-built messages: variants nested exactly `MAX_DEPTH`
  (32) deep round-trip; one deeper closes the connection; a header announcing 1 MiB of body
  closes it before the body; the server still serves afterwards. SASL: over TCP with ANONYMOUS
  off nothing is offered; an over-long line closes the connection; on the Unix socket EXTERNAL
  succeeds for this uid only. The encoder holds the same depth bound.

Mutation-checked: removing the decoder's depth check fails the depth test; removing the
fail-closed answer makes dbus-send report NoReply after 20 s.

Why the bound is 32 and not the specification's 64: a 64-deep echo was refused by NetGet's
event pipeline (64 JSON levels) and answered with the fail-closed error. See the server's
AGENTS.md.

Peers: `apt-get install dbus-bin libglib2.0-bin python3-dbus-next`; every test fails rather
than skips without them. CI: the `dbus-pairs` job in `protocol-pairs.yml`.
