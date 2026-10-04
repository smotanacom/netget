# Zenoh router/peer — Experimental

Built on the zenoh 1.10.1 runtime (feature `zenoh` enables the crate with only the TCP transport
and its `unstable` link information). `node.rs` is shared with the client: session configuration
(multicast scouting off; only the given listen or connect endpoint), sample/query/payload JSON,
validation, and running the handler's put, delete, get and reply actions on a session.

Rust owns:
- The session in `router` (default; routes between the clients that connect) or `peer` mode,
  listening on `tcp/<bind>`; the port is read back from the session's locators.
- Subscribers for the `subscribe` key expressions and queryables for `queryable`, both limited to
  remote origins so NetGet's own traffic does not loop back.
- Each sample becomes `zenoh_sample`; each query `zenoh_query` (64 in flight; past that, the
  query is refused as busy). Replies (`zenoh_reply`, `zenoh_reply_error`, several allowed, none
  is an empty answer) go to the query; `zenoh_put`, `zenoh_delete` and `zenoh_get` run on the
  session, and a get's replies (10 s, 256 replies) become `zenoh_get_result`, chained at most 4
  deep.
- A handler that cannot answer a query fails it closed with a reply error carrying only a
  category. The session's links are polled every second into the connection list.

There is no per-connection push (the handler acts on the session), so the server is
`request_only`. No TLS, QUIC, UDP or shared memory; no storage.
