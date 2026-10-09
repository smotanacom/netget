# Graphite Carbon plaintext emitter

Feature `graphite`; registry `Graphite`; keywords `graphite`, `carbon`; TCP 2003.
Experimental; native structured codec shared with the collector.

`graphite_connected` fires once after TCP connection. Static/script/manual/model handlers
and injected `send_graphite_batch` actions share validation and writing. Each action has
`metrics: [{path,value,timestamp}]`: 1..256 metrics, <=4096 encoded bytes per line (excluding
LF), <=64KiB total. The entire action validates before any bytes are written. `timestamp=-1`
asks Carbon to use receiver time; tagged paths are opaque strings. See the server codec/docs
for exact field restrictions.

The command channel exists before the connected handler runs, so injection/disconnect works
while a manual handler is parked. Restored client memory is passed into the connected event.
Connect and write deadlines are 10 seconds. A write failure closes the stream because a
partial write cannot safely be replayed; no retry is attempted. Remote EOF/read errors and
unexpected response bytes also close the emitter and remove its command handle. Stop and
`disconnect` drop both TCP halves and cancel pending connected-handler work.

Successful send reports bytes accepted by the local transport, not remote persistence.
Carbon plaintext has no acknowledgments. TCP only; no UDP, Pickle, TLS/authentication,
storage/query API or aggregation. See `tests/client/graphite/AGENTS.md` for peer evidence.
