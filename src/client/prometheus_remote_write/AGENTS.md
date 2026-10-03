# Published Remote Write 1.0 emitter

Experimental typed `write_remote_samples` accepts `{batch:{series:[{labels:{name:value},
samples:[{timestamp_ms:i64,value:number|nan|+inf|-inf|stale}]}]}}`. The shared native
collector codec sorts action-map labels and validates the entire batch before IO.
The receiver documentation states scalar and body bounds and special-float rules.
Actions contain no raw protobuf/compressed bytes. The caller supplies per-series
cross-batch timestamp order and stale markers at the appropriate lifecycle point;
this emitter does not scrape, discover targets or infer series disappearance.

`remote_addr` is a cleartext HTTP origin, default port9090; `path` separately sets
the absolute endpoint (default `/api/v1/write`), without query/fragment.
Optional `auth_token` supplies Bearer auth. Reserved protocol headers are fixed;
there is no custom-header override. No TLS/basic/cloud/proxy auth or redirects.

`remote_write_connected` starts the logical session without dialing. Injected
commands and disconnect remain serviceable while a common handler is parked or a
write is waiting/retrying. `remote_write_response` reports terminal status,
accepted(any2xx), attempts and submitted series/sample counts; durable storage is
never confirmed by HTTP alone. Version1.0 requires ignoring the reserved response
body, so even binary/nonempty successful bodies are drained within a64KiB bound
and discarded. No text/error-message schema is inferred.3xx and4xx are terminal,
except429 when `retry_429=true` (default false). Retry-After is not interpreted.

5xx and transport/framing failures retry the identical batch while connected,
with exponential backoff100ms doubling to5s. Each exchange is bounded10s with
64headers/32KiB aggregate header limits. There is one in-flight batch, held only
in volatile memory; no WAL or persistent queue. A transport error may happen after
remote acceptance, so retries can duplicate remotely. Disconnect/removal cancels
both owned IO and backoff and loses unsent samples. A caller command timeout does
not promise remote cancellation; disconnect or remove the client to cancel it.

The logical task owns its common handler future,32queued events,32queued handler
actions and followup depth8. Capacity closes the session; cleanup drops these
futures before marking Disconnected. Typed validation rejects a command without
sending bytes. Concurrent writes are rejected while one is active. Common memory
and deterministic handlers use the existing dispatcher; no protocol store is added.
Local address is logical0.0.0.0:0; each HTTP attempt owns a fresh TCP connection.

Required official Prometheus3.15.0 receiver tests read finite/special/stale values
back from its real temporary TSDB and verify a400 duplicate-value rejection.
The opposite-role test drives the real official sender too. These peers are
external Apache-2.0 executables/packages, never native framing dependencies.
No full-agent/2.0, metadata/exemplars/native histograms, durability, deduplication,
automatic stale detection, scraping, queries, fuzz or capture evidence is claimed.
