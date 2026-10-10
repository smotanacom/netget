# SunRPC client

A portmapper/rpcbind caller over one TCP connection. It uses the server's `wire.rs`: record
marking, and AUTH_NONE calls to program 100000. Replies are matched by xid; a stale reply to a
timed-out call is skipped. One call is in flight at a time, with a 10 s deadline. A transport
failure ends the session.

## Actions

Each answers `sunrpc_reply{operation, ok, result, error?}`:

- `sunrpc_null{version}`
- `sunrpc_dump{version}`: v2 gives `{program, version, protocol, port}`; v3/v4 give
  `{netid, address, port, owner}`. Both carry `program_name` when the program is well-known.
- `sunrpc_getport{program, program_version, protocol}`: PMAP v2; 0 when not registered.
- `sunrpc_getaddr{…, protocol, version}`: RPCBIND; `{address, port}`.
- `sunrpc_set{…, port, version}`: v2 takes tcp/udp; v3/v4 take a netid, register the
  universal address on loopback and owner `netget`.
- `sunrpc_unset{…}`: v3/v4 with an empty protocol removes every transport.
- `sunrpc_gettime`
- `disconnect`

`ok` is false for an RPC-level error (`PROG_UNAVAIL` …) and for a set/unset the server
answered `false`. `actions::call` validates every action before anything is sent.

A handler chain stops after `MAX_FOLLOWUP_DEPTH` (8).
