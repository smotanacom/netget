# Published Remote Write 1.0 collector

Experimental, separate from the Prometheus scrape/exporter protocol. Native
protobuf implements the published April2023 `prometheus.WriteRequest`, `TimeSeries`,
`Label` and `Sample` schema; Snappy uses block format, including all copy forms.
No third-party remote-write implementation is linked and no dependency is added.
Primary references are the [published1.0 specification](https://prometheus.io/docs/specs/prw/remote_write_spec/),
[Prometheus3.15.0 schema](https://github.com/prometheus/prometheus/blob/v3.15.0/prompb/types.proto)
and [Snappy wire description](https://github.com/google/snappy/blob/main/format_description.txt).

POST the configured absolute path (default `/api/v1/write`). Required headers are
`Content-Type: application/x-protobuf` (optional `proto=prometheus.WriteRequest`),
`Content-Encoding: snappy`, `X-Prometheus-Remote-Write-Version: 0.1.0` and a nonempty
User-Agent of at most512bytes. MIME/coding/auth scheme tokens are case-insensitive;
protobuf message identifiers are case-sensitive. Duplicate required headers, v2
message/version, query parameters and unsupported media types fail closed.
Optional `auth_token` checks one Bearer credential; credentials never enter events.
`llm_fallback` defaults false, explicit common handlers always run.

`remote_write_request` exposes typed series labels and float samples with signed
Unix millisecond timestamps. Finite values are JSON numbers; other values are
`nan`, `+inf`, `-inf` or `stale`. Stale is exactly the special NaN bits
0x7ff0000000000002; other NaN payloads normalize to `nan`. Label names follow the
legacy1.0 ASCII grammar, values are nonempty UTF-8, labels arrive sorted and unique.
Metric names are checked when `__name__` exists; that label is recommended by v1,
not mandatory here. Samples must be timestamp ordered within each series; repeated
series label sets and sample-less series are rejected. An empty entire request is
a valid v1 negotiation probe. Singular duplicate protobuf fields are rejected.
Unknown/reserved fields are bounded, counted and discarded without byte exposure;
metadata is not ingested. Exemplars and native histograms in a TimeSeries fail400.

`accept_remote_write_samples` returns204; `reject_remote_write_samples` allows400,
429,500,503 and bounded text, with optional1..3600 numeric Retry-After on429/503.
An empty successful or common-only handler accepts. Backend/action failures or
multiple decisions return503 even alongside a valid accept action. Common actions
that ran before failure are not rolled back. Default observation uses no model.
Acceptance means handler observation, not durable ingestion, TSDB storage or
exactly-once delivery. The protocol has no domain database; use common memory or
explicit generic facilities for application data.

Bounds:256KiB wire and decoded body;128series,2048total samples,32labels per series;
128byte names,2048byte values,32768protobuf fields;1024byte token/path;
256connections through the shared limiter,64headers/32KiB aggregate headers;
30s header/body deadlines and10s decision-write deadline. All tasks are registered
with AppState; removal cancels parked handlers and sockets. HTTP/1.1 uses a fresh
connection for each request and closes it after the response. Terminal decisions
are logged; HTTP errors reveal no backend details.

Required independent tests use official Prometheus3.15.0 as a real sender and
receiver, with its own TSDB readback and official exporter0.22.1 scrape values.
See tests documentation for pins and bootstrap. No full1.x/2.0 conformance,
metadata/exemplar/histogram support, cross-request ordering validation, queries,
authenticated TLS/basic/cloud auth, durable store, fuzz or capture claim.
