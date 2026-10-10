# D-Bus client tests

No LLM calls. `real_server_test.rs` runs **dbus-daemon** unprivileged from a temp config
(Unix socket and TCP; EXTERNAL and ANONYMOUS) and a **python dbus-next** service,
`net.example.Service`: Greet returns `hello, <name>`, Record appends its argument to a file and
emits `Announced`, Slow sleeps 3 s.

- The chain: own `net.netget.Client`, add a match for the service's signals, call Greet; on
  the reply, call Record with the greeting plus " acknowledged". The service's own file then
  reads `hello, netget acknowledged`, and the Announced signal arrives as `dbus_signal`.
- **dbus-send** calls the name NetGet owns through the bus: Ping is answered by the model;
  Silent gets the fail-closed `org.freedesktop.DBus.Error.Failed`.
- Injected: a method the service lacks (UnknownMethod from dbus-next) and Slow against a 1 s
  `timeout_ms` (NoReply).
- Over TCP the client falls back to ANONYMOUS and ListNames includes its unique name.

Mutation-checked: dropping the model's actions and removing the fail-closed answer each fail
the chain test. Peers: `apt-get install dbus-daemon dbus-bin python3-dbus-next`; fails rather
than skips without them.

The bus config disables AppArmor mediation (`<apparmor mode="disabled"/>`): on a host with
AppArmor (GitHub's Ubuntu runners), dbus-daemon asks it about every connecting peer, the query
fails for a TCP socket, and the connection is dropped mid-authentication. A host without
AppArmor never shows this.
