Based on async-opcua-server 0.19.0, MPL-2.0 (upstream source notices retained).
NetGet changes in src/server.rs: cap accepted TCP connections at 256; stop accepting
on cancellation; abort all owned connection tasks when Server is dropped. This makes
NetGet's registered task cancellation close accepted sockets rather than detach them.

In src/session/controller.rs, cap pending services at 64 and abort their owned
JoinHandles on drop, so handler work cannot outlive a stopped connection.
