# SunRPC server (the portmapper / rpcbind)

ONC RPC (RFC 5531) program 100000: PMAP v2 and RPCBIND v3/v4 (RFC 1833), on TCP and UDP at
the same port (well-known 111, so `PrivilegedPort(111)`). Hand-rolled in `wire.rs`, which the
client shares.

## What Rust answers

- Malformed calls: `GARBAGE_ARGS` with the call's xid, when one can be read.
- RPC version ≠ 2: `MSG_DENIED`/`RPC_MISMATCH` 2–2.
- Another program: `PROG_UNAVAIL`. Another version: `PROG_MISMATCH` 2–4.
- `CALLIT`/`INDIRECT`, `UADDR2TADDR`/`TADDR2UADDR`, `GETSTAT`: `PROC_UNAVAIL`.
- `NULL` (every version) and `GETTIME` (v3/v4, the server's clock).
- Bounds:
  - a TCP record of `MAX_RECORD` (256 KiB) in at most `MAX_FRAGMENTS`, refused from the
    header before anything is read;
  - XDR strings ≤ `MAX_STRING`, auth bodies ≤ 400;
  - at most `MAX_MAPPINGS` in an answer;
  - an idle TCP connection closes after 120 s;
  - at most `MAX_UDP_IN_FLIGHT` UDP calls handled concurrently.

## What the model answers

NetGet stores no registrations: the model holds them and answers lookups.

- `sunrpc_query {transport, rpc_version, procedure, program?, program_name?, program_version?,
  protocol?}`, for dump, getport, getaddr, getversaddr and getaddrlist. The model answers
  `sunrpc_mappings{mappings: [{program, version, protocol, port, owner?}]}`. It may give the
  whole table; Rust picks what each procedure returns:
  - getport, getaddr: the exact version, else any version of the program (as rpcbind does);
  - getversaddr, getaddrlist: the exact version only.

  Universal addresses use the address the client reached NetGet on: `h1.h2.h3.h4.p1.p2`, or
  `::1` for tcp6/udp6 when reached over IPv4.
- `sunrpc_register {operation set|unset, …, port, address, owner, credentials}`: the model
  answers `sunrpc_accept` or `sunrpc_reject`, which become the XDR `bool` the caller gets.
  Credentials are decoded: `{flavor: none}` or AUTH_SYS `{machine, uid, gid, gids}`. An unset
  with an empty netid means every transport.

A failure of any kind (an LLM error, an invalid reply, silence) answers `SYSTEM_ERR`, logged
`decision=fail_closed_*`/`model_silent`. The caller is never left waiting and never handed a
made-up mapping.

UDP datagrams are not tracked as connections (each is one call); TCP connections are.
