# Zipkin client

A reporter: `zipkin_report {spans, gzip}` POSTs JSON v2 to `/api/v2/spans`, and
`zipkin_query {endpoint, trace_id, query}` GETs the read API. Every request opens its own
HTTP/1.1 connection (hyper, `Connection: close`, 10 s deadline), so there is no session to
keep; `disconnect` ends the reporter.

Spans are validated and canonicalised with the server's `wire.rs` before sending (an invalid
span or an empty report is refused locally, nothing sent), and an answer is decoded and
checked against its endpoint's shape before it reaches the handler: `zipkin_report_result
{status, accepted, span_count, message}` and `zipkin_query_result {endpoint, status, result,
message}` — a non-200 or malformed answer has `result: null` and says why in `message`.

Bounds: 1 MiB per body (after gzip too), 1000 spans, and a handler chain stops after 8
follow-ups (`MAX_FOLLOWUP_DEPTH`; tested by a handler that queries on every result).
Cleartext only; the collector is `host:port` or `http://host:port` (default port 9411).
