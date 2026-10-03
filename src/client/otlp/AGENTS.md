# OTLP semantic exporter

Feature `otlp` registers one client named `otlp`. It exports a bounded typed subset
of traces, metrics and logs over OTLP/gRPC or OTLP/HTTP protobuf. The model supplies
telemetry fields; NetGet builds generated protobuf messages, drives the transport,
and reads the receiver's acknowledgement. No raw protobuf, base64 or OTLP JSON
payload action exists. The protocol creates no telemetry file, spool or domain store.

`actions.rs` declares actions/events/parameters, `wire.rs` validates and builds messages,
and `mod.rs` owns the connection, command channel, export and handler futures.

## Connection and authentication

`remote_addr` is `host:port` or `[IPv6]:port`; use 4317 for ordinary gRPC collectors
and 4318 for ordinary HTTP collectors. `transport` defaults to `grpc`; `http` uses a
reusable HTTP/1.1 connection. `tls` defaults to true. Explicit false selects cleartext,
and rejects unused TLS CA/name parameters. Certificate-chain and hostname verification
are always enabled for TLS. Public WebPKI roots are supplemented by the optional PEM
`ca_cert_path`; `server_name` defaults to the receiver's hostname. The CA path must name
a regular file, read on a blocking worker and bounded at 1 MiB. No keychain loading runs
on a Tokio worker.
There is no skip-verification switch, mTLS, HTTP proxy or automatic reconnect.

HTTP uses a directly owned Hyper sender/connection future; gRPC uses the generated
opentelemetry-proto 0.29 services with tonic 0.12/prost 0.13. A duplicate socket guard
forces shutdown on normal exit, connection failure or task removal, including tonic's
internally spawned transport driver. The gRPC connector consumes that sole owned socket
and refuses subsequent automatic reconnection attempts. Actual local/peer addresses
and transport/TLS facts are recorded in client state.

The command channel is registered before `otlp_connected`. One registered owner polls
all exports and shared static/script/manual/model handlers. A parked handler leaves
command injection and disconnect responsive. At most 16 exports plus handlers exist
in total, with a handler slot reserved by each export. Handler action lists also have
16 entries maximum. Automatic export chains stop after four follow-up levels; the final
response still reaches the handler, and disconnect remains usable at that bound.
Shared common actions, including set_memory, execute before returned follow-up actions.

## Typed actions

All export actions require `service_name` (1..256 UTF-8 bytes), optional `scope_name`
(at most 256 bytes, default `netget`), and optional flat `resource_attributes`.
Use service_name rather than supplying a second resource attribute named service.name.
Attributes accept strings, bools, signed 64-bit integers or finite floats. Each object
has at most 32 keys; keys have 1..256 bytes and string values at most 1024 bytes.
Arrays, nested maps, null and binary attributes are refused.

| Action | Fields |
|---|---|
| `export_otlp_traces` | 1..128 spans, each with name, nonzero 32-hex trace_id, nonzero 16-hex span_id, positive u64 start/end Unix nanoseconds and end>=start; optional parent_span_id, kind internal/server/client/producer/consumer, status unset/ok/error, status_message and attributes |
| `export_otlp_logs` | 1..128 text logs, each with body 1..4096 bytes and positive time_unix_nano; optional severity 0..24 (default 9), severity_text, paired trace/span IDs and attributes |
| `export_otlp_gauge` | One named gauge, optional description/unit, 1..128 data points with positive time_unix_nano, numeric value and attributes |
| `disconnect` | Cancel exports and handlers, then shut down the socket |
| `wait_for_more` | No export |

Span names are at most 256 bytes and status messages 512; metric names/descriptions 256,
units 64; log severity text 64. Aggregate typed text and encoded protobuf are each bounded
at 1 MiB before a transport send. IDs are semantic hexadecimal identifiers, not wire payloads.
Exported timestamps are explicit input; NetGet does not invent historical telemetry.

## Events and failures

`otlp_connected` carries remote_addr, transport, tls_verified and authenticated server_name
when TLS is enabled. `otlp_export_result` carries signal, transport, service_name, item count,
result (`accepted`, `partial_success`, `rejected`), rejected count, a diagnostic cut to 512
bytes, retryable, optional retry_after_secs, and either HTTP status or gRPC code.
Partial-success counts must be nonnegative and no larger than the exported count.
A zero-count nonempty partial-success message is retained as a warning. Partial success
is never retryable. HTTP retryability follows 429/502/503/504; gRPC RESOURCE_EXHAUSTED
is retryable only with a valid RetryInfo recovery delay. Other gRPC status classes follow
the [OTLP specification](https://opentelemetry.io/docs/specs/otlp/).
HTTP Retry-After exposes integer seconds; date-form delays are outside this subset.

Local transport, HTTP response-decoding and outer deadline errors raise `otlp_export_error`.
Generated gRPC codecs can return a status without an error source for an invalid response;
that status is reported as a nonretryable rejection. `grpc_code` therefore includes library
decoder statuses as well as receiver refusal statuses.
Exports injected through AppState return honest `Executed` verdict details, validation
rejections, or errors; they do not claim encrypted wire byte counts. There are no automatic
retries. A caller decides whether to retry after inspecting the semantic result.
Request/response counters count acknowledged exports; local failures do not invent replies.

Requests optionally use gzip (`gzip=true`, default false). Response size is at most 4 MiB,
including after gzip decompression. HTTP bounds raw body collection and inflation separately;
gRPC uses the pinned tonic receive-limit patch, including on decompression. Response headers
are bounded 32 KiB (HTTP/1.1 also at most 128 fields). The connection deadline defaults 10 s
(range 1..60), whole export/response deadline 30 s (1..300), and inactivity with no export or
handler 120 s (1..3600). Queue wait is inside the export deadline.

## Evidence and scope

`tests/client/otlp` requires the official core Collector 0.162.0, including real decoded
traces/gauge/log records over both transports, with plain/gzip and authenticated TLS. It
also exercises native pairing, partial/refused responses, retry hints, shared script/memory,
allocation/deadline/cancellation bounds and responsive command injection. Missing peers fail.

Experimental. No second independent receiver, pcap oracle or fuzz target. No sum,
histogram, exponential histogram, summary, exemplars, span events/links, profiles, binary
logs, JSON export, persistent batching/spooling, automatic retries/reconnect or mTLS.
