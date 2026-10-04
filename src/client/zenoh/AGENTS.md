# Zenoh client — Experimental

Uses `src/server/zenoh/node.rs`. Opens a session in `client` (to a router) or `peer` mode
connecting to `tcp/<remote_addr>`, waits up to 10 s for the link (a peer opens before it is up),
and reports `zenoh_connected` with its id and links. Declares subscribers and queryables from
`subscribe` and `queryable`; samples, queries and get results are handler events in order, and
a query waits for the handler's replies. Actions: `zenoh_put`, `zenoh_delete`, `zenoh_get`,
`disconnect` (`zenoh_reply`/`zenoh_reply_error` only answer a query). No TLS, QUIC or UDP.
