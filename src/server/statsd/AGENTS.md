# StatsD / DogStatsD collector

Feature `statsd`, registry name `StatsD`, keywords `statsd` and `dogstatsd`, UDP port 8125.
Experimental: source and wire tests do not satisfy the project's six Stable conditions.
The implementation uses Tokio and a native codec with no added dependency.

`dialect` defaults to `dogstatsd`; `statsd` restricts the accepted grammar to classic metrics.
`llm_fallback` defaults to `false`: unmatched valid datagrams go directly into the existing
bounded access log. Configured script, static, manual and LLM handlers always run through the
standard dispatcher; `llm_fallback: true` additionally sends unmatched batches to the model.
An instruction by itself does not opt the collector into model reasoning. This is deliberate
for high-rate telemetry and appears in startup docs/metadata. One datagram raises one
`statsd_batch`, never one event/task/model call per metric.

The event exposes `records`, `record_count`, `source_addr` and `dialect`. Records are tagged
`kind: metric|event|service_check`; see the public serde types in `codec.rs` and action docs.
`collect_statsd_batch` means observation in the access log, not aggregation or persistence.
Use generic server memory/SQLite through handlers if desired; this protocol owns no database.

Supported surface:

- Classic counters `c`, gauges `g`, timers `ms`, sets `s`. Values are strings to preserve gauge
  delta signs and set members. `-2|g` is a delta, not an absolute negative gauge.
- DogStatsD adds histograms `h`, distributions `d`, tags, events and service checks.
- DogStatsD sample rates in `[0,1]` on counters/timers/histograms/distributions are metadata,
  following the Datadog wire documentation. Classic StatsD requires `(0,1]` because reference
  collectors divide by the rate. NetGet neither samples nor rescales/aggregates values.
- Event titles/text use UTF-8 byte lengths, permit pipe characters and decode escaped
  newlines. Optional timestamp, hostname, aggregation key, priority, source type, alert type,
  tags. Service-check status 0..3, timestamp/hostname/tags, final message with escaped newlines
  and `m\:` handling. Unknown options, duplicate options and invalid numeric values fail.

Bounds and lifecycle:

- Max 8192 bytes and 256 records per datagram; a final newline is accepted within the cap.
- One 8193-byte receive buffer catches oversize/truncated datagrams before parsing their
  prefix. Invalid UTF-8 and any malformed record reject the entire datagram atomically.
- Processing is sequential, with no unbounded per-packet task queue. Under a slow handler,
  the OS UDP buffer may drop subsequent packets. This is a diagnostic collector, not a
  reliable high-throughput ingestion service.
- Only the currently processed batch gets a peer row, removed afterward. The receiver task
  is registered; stop aborts its active handler and releases the socket.
- No response is sent on any path: StatsD has no acknowledgment/error message. Handler
  failures appear in access/status logs (`decision=fail_closed_handler_error`). Invalid
  datagrams log lengths/source/error, not arbitrary undecodable bytes.

Explicit limits: no statistical aggregation, persistence, TCP, Unix datagrams, authentication,
retransmission, packed metric values, origin/container metadata, metric timestamps, cardinality
or proprietary options. Names in the implemented metric grammar use ASCII letters/digits,
underscores and periods. Restrictive validation is not a claim to accept every StatsD dialect.

References: [Datadog wire format](https://docs.datadoghq.com/extend/dogstatsd/datagram_shell/),
[StatsD reference implementation](https://github.com/statsd/statsd),
[Datadog Python emitter](https://github.com/DataDog/datadogpy).
Evidence and commands: `tests/server/statsd/AGENTS.md`.
