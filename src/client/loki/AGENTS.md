# Loki push emitter (Experimental)

A logical cleartext HTTP origin (`host:port` or `http://host:port`, default3100) opens no TCP
until an entire typed `push_loki_entries` batch passes validation. Origins exclude credentials,
path, query, fragment, TLS and proxies. Optional startup Bearer token is private to transport;
each batch has optional single `tenant_id`, carrier `json`(default)/`gzip_json`/`snappy_protobuf`,
and typed labels/entries/metadata documented in the collector. The batch validation and body
bounds apply before any socket opens. Native protobuf encoding emits valid raw Snappy literal
blocks; decoding supports all copy forms. Fresh TCP per write; logical local address0.0.0.0:0.

`loki_connected` and `loki_push_response` use common client handlers/shared memory. Responses
include tenant, carrier, submitted stream/entry counts, HTTP status, optional bounded error
message and numeric retry advice.204 is the only accepted status;260 means blocked ingestion.
Supported error statuses400/401/403/404/405/408/413/415/422/429/500/503 keep the logical session
open. No automatic retry, partial-acceptance count, atomicity or persistence promise. HTTP200
can mean blocked ingestion in Loki configuration; this client rejects that ambiguous status.
Malformed/oversized/unsupported responses and IO/deadline errors close the logical session.

One registered owner task polls the HTTP driver inline with the exchange and common-handler
future. Live injection remains available while a handler parks or a write is pending. One
exchange at a time; concurrent injected writes are rejected. Disconnect/removal cancels the
socket/driver/handler and pending command. Queues cap32 events/actions; followups cap8. A
handler exceeding its action cap closes before opening a socket. Memory refreshes per event.

Request bounds match the collector. Response cap64KiB, error4096 UTF-8 bytes (only tab/CR/LF
controls),32KiB/64 headers,10s whole exchange; identity responses only, numeric Retry-After
1..3600 only429/503.204 must have empty body, no Transfer-Encoding/Retry-After and absent/zero
Content-Length. No redirects, TLS, proxy/basic/cloud auth, HTTP-date retry, order/retention
engine, query API, durable store, private domain state, OTLP, retry, fuzz or pcap.

Examples and maturity live in actions.rs. Independent official Loki service readback and
maintained Alloy/Python emitters are required by the Linux/macOS peer suite; see the test
docs. Current bootstrap supports Linux amd64 and macOS arm64 explicitly. Other platforms
have native codec/pair coverage, without an official-daemon evidence claim.
