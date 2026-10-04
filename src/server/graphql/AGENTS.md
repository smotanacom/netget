# GraphQL server — Experimental, GraphQL over HTTP (draft spec), queries and mutations

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
GraphQL request at all). Subscriptions are refused (a separate roadmap item).

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

No resolvers or storage in Rust; no batching, persisted queries, uploads, `@defer`/`@stream`.
