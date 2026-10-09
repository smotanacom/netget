# GraphQL server — Experimental, GraphQL over HTTP (draft spec) and graphql-transport-ws

hyper HTTP/1.1 at `endpoint` (default `/graphql`). `engine.rs` (shared with the client) is
apollo-compiler 1.33: the `schema` startup parameter (SDL, ≤256 KiB, needs a Query type) is
validated at startup; every document is parsed (depth 64, 15 000 tokens, 64 KiB) and validated
against it, the operation is selected and variables coerced — each failure a request error with
locations, in the spec's order.

HTTP rules (GraphQL-over-HTTP): POST needs `application/json` (else 415); GET reads
`query`/`operationName`/`variables` from the URL and never runs a mutation (405, `Allow:
POST`); other methods 405, other paths 404. `Accept` picks the response type: absent →
`application/json`; the highest-q of `application/graphql-response+json` / `application/json`,
wildcards meaning the former; neither acceptable → 406. A request error is 400 under
graphql-response+json and 200 under legacy application/json (400 for a body that is not a
GraphQL request at all). A subscription over HTTP is a request error pointing at the socket.

- Introspection-only operations (`__schema`, `__type`, `__typename`) are answered by Rust with
  no handler call; `introspection: false` makes `__schema`/`__type` field errors.
- Everything else raises `graphql_operation` with the root fields (arguments with variables
  substituted) and a `shape` skeleton of the data to return. `graphql_result {data, errors}` is
  executed by apollo's executor over that data: lookups by response key then field name,
  `__typename` picks a union/interface member (or the only one), result coercion, null
  propagation, field errors with paths; handler errors are appended. `graphql_error` answers
  `data: null` with one error (`model_reject`).
- No answer, two answers, an invalid answer or a backend failure is 503/500 with a category
  message and no data (`fail_closed_*` / `model_silent`) — never invented data.

## Subscriptions — `ws.rs`, graphql-transport-ws on the same endpoint

A GET with `Upgrade: websocket`, version 13 and the `graphql-transport-ws` subprotocol gets a
101 from hyper; the upgraded socket then runs inside the same connection task (so it keeps the
connection's permit and task registration). No subprotocol, or only the legacy `graphql-ws`
(subscriptions-transport-ws), is a 400. Lifecycle per the graphql-ws protocol:
`connection_init` within `connection_init_timeout_secs` (default 10) or close 4408, always
acknowledged (no auth hook); a second init 4429; `subscribe` before the ack 4401; a duplicate
active id 4409; unparsable or unknown messages 4400; `ping` → `pong`. A refused document is an
`error` message with the request errors. A query or mutation over the socket is one `next` and
`complete`. A subscription raises `graphql_subscription_start` (root field, variables, shape);
the handler answers zero or more `graphql_event` (executed over the Subscription type, each a
`next`), optionally `graphql_complete`, or `graphql_error` (an `error` message). Later events
come through the connection's peer handle (`send_to_peer` with `graphql_event`,
`graphql_complete` or `disconnect`), which names the subscription id; an id that is not active
is refused. A client `complete` ends the subscription. A backend failure on start is an `error`
with a category message and frees the id. 64 subscriptions per socket, 1 MiB messages.

hyper writes response header names in lower case; header names are case-insensitive, but gql
4.4.0 looks up `Sec-WebSocket-Protocol` case-sensitively and, offered both subprotocols, falls
back to the legacy protocol when it misses. Configure gql with `graphql-transport-ws` only.

No resolvers or storage in Rust; no batching, persisted queries, uploads, `@defer`/`@stream`, SSE.
