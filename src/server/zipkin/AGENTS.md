# Zipkin server

A Zipkin collector and read API over HTTP/1.1 (hyper, the InfluxDB collector's shape): one
request per connection, `POST /api/v2/spans` with JSON v2 bodies (identity or gzip) and
`GET /api/v2/{services,spans,remoteServices,traces,trace/{id},traceMany,dependencies,
autocompleteKeys,autocompleteValues}`. Port 9411.

## Spans (`wire.rs`, shared with the client)

Validated and canonicalised the way zipkin-server 3.5.1 does it, measured against the jar:
ids are lower-case hex with no prefix, left-padded to 16 (a trace id longer than 16 to 32),
never all zero; `kind` is CLIENT/SERVER/PRODUCER/CONSUMER; unknown keys are ignored; a scalar
tag value is kept as its text; an unparseable endpoint address and a non-positive timestamp
or duration are dropped, not refused. Keys come out in Zipkin's own order. A malformed span
is a 400 naming it, before any handler runs.

## What the handler decides

NetGet stores no spans: the handler (a script's file, or the model's memory) is the storage.
`zipkin_spans {spans, span_count, services}` → `zipkin_accept` (202) or `zipkin_reject
{status, message}` (plain text, 400/403/404/413/429/500/503); no answer accepts, because a
collector accepting a report asserts nothing about anyone. `zipkin_query {endpoint, trace_id,
query}` → `zipkin_query_result {result}` or `zipkin_reject`; the result is checked against the
endpoint's shape (`wire::result`: names, a trace, traces, dependency links) and a wrong shape
is never sent. An empty report is accepted with no handler call.

## Failure modes and bounds

A handler failure, or silence or a wrong answer to a query, is 503 with `Retry-After: 5` and
`WireFailure`'s text (`answers_on_failure`), with its `decision=` tag. Bodies are capped at
1 MiB before and after gzip (a gzip bomb is a 413 by its decompressed size), 1000 spans per
report or answer, 256 tags and annotations per span, 64 KiB per string; 32 KiB / 64 headers;
30 s header and body deadlines; 256 connections. A query parameter the endpoint does not take,
or one given twice, is a 400. Not implemented: proto3 and Thrift (415), the v1 API (404), TLS,
the UI.
