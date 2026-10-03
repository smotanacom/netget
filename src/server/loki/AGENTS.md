# Loki push collector (Experimental)

Native `POST /loki/api/v1/push` over cleartext HTTP/1.1. Accept JSON (`application/json`),
gzip JSON (same MIME with `Content-Encoding: gzip`), or raw Snappy block compressed public
push protobuf (`application/x-protobuf`, absent or explicit `snappy` HTTP encoding; identity is also tolerated). Both type and encoding
headers must be unique; content type is required. No query parameters, alternate endpoints,
OTLP, framed Snappy, HTTP/2, TLS or storage/query/retention/order engine.

Events expose `tenant_id`, `tenant_provided`, carrier, typed streams `{labels,entries}` and
entries `{timestamp_ns,line,structured_metadata}`, plus source/authentication facts. JSON
wire timestamps are decimal **strings**; typed timestamps are signed i64 nanoseconds.
Protobuf Timestamp seconds/nanos are checked before conversion. Labels and string metadata
use ASCII identifier keys. Stream label names starting `__`, duplicate keys/stream sets,
non-string metadata, malformed UTF-8, unsupported push fields and invalid timestamps fail
before dispatch. Valid lines can contain UTF-8 controls and newlines; label values use
ordinary JSON or Go quoted-string escapes. Entry ordering is preserved, without history.

One valid `X-Scope-OrgID` identifies a tenant: <=150 ASCII bytes, alphanumerics or `!-_.*'()`,
excluding `.` and `..`; multi-tenant `|` and colon are rejected. Absent header is `fake` unless
`require_tenant=true`, which returns401. Identity does not grant a tenant ACL or create a
private tenant store. Optional `auth_token` checks exactly one `Authorization: Bearer ...`
with a constant-work compare; no configured token means anonymous. This is a proxy-style
check; Loki itself separates authorization from this API. Tokens never reach events/logs.

Unmatched pushes collect through the common access log without a model call. A configured
handler runs the ordinary dispatcher and shared memory; `llm_fallback=true` opts unmatched
pushes into model calls. `accept_loki_entries` returns204, indicating handler observation,
without a durability promise. An empty successful answer also accepts. `reject_loki_entries`
returns a bounded plain-text error:260(blocked ingestion),400/401/403/404/413/415/422/429/500/503;
optional numeric Retry-After1..3600 only429/503. No partial-accept action or rollback guarantee.
Failed actions (even alongside acceptance), dispatch errors and multiple decisions return503
and an explicit failure access log. Protocol errors answer directly and never dispatch.

Bounds:256KiB wire and decoded body,64 streams,1024 total entries,16KiB line,32 labels and64
metadata pairs,128-byte keys,2048-byte values,1024-byte printable ASCII token; JSON nesting8,
16384 protobuf fields (encoder and decoder). Snappy validates all literal/copy forms,
backreference overlap, offsets, exact decoded length, preamble and trailing input. Gzip
bounds decompression and rejects truncation/trailing junk.256 live connections with503/refusal,
32KiB/64 headers,30s header and body deadlines;10s response-write deadline starts after a
handler decision so parked manual work has no transport deadline. One request per TCP
connection. Listener/connections are owned by AppState; stop aborts sockets and intercepts.

Primary wire references: [Loki API](https://grafana.com/docs/loki/latest/reference/loki-http-api/),
[pinned schema](https://github.com/grafana/loki/blob/v3.7.8/pkg/push/push.proto),
[tenant rules](https://grafana.com/docs/loki/latest/operations/multi-tenancy/),
[Snappy block format](https://github.com/google/snappy/blob/main/format_description.txt).
Implementation is native and uses only existing flate2; no Loki/Alloy/Snappy runtime code is
vendored or linked. Independent peer commands/licensing are in tests/server/loki/AGENTS.md.
Experimental: no fuzz/pcap evidence and no general Loki service compatibility claim.
