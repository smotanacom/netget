# InfluxDB v2 write emitter

HTTP-only logical session, one fresh HTTP/1.1 TCP exchange per typed write. Use ordinary
client event handlers, memory and command injection. Poll the HTTP connection driver inside
the owned session future; never detach it. Disconnect/remove-client must cancel a parked
handler and an in-flight exchange. Validate the whole batch before opening a socket.

The token is a startup credential only, never an action field/event/access-log value. Emit
`Authorization: Token ...`, `/api/v2/write?org=...&bucket=...&precision=...`, text/plain UTF-8,
and identity/gzip. Require empty 204 success or bounded typed JSON error; expose HTTP status,
partial-write counts/line and numeric Retry-After without retries. No TLS, redirects, query
API, durable queue, HTTP-date retry advice or server persistence claims.

Independent receiver uses unmodified InfluxData line-protocol v2.2.1 decoder inside a small
test HTTP adapter which validates route/query/auth/encoding/response semantics. It is a
decoder-backed receiver, not an official InfluxDB daemon. The additional mandatory Linux/
macOS official 2.9.1 service test closes the service gap, with independent Python readback
for all five kinds and four timestamp precisions plus real401/404 responses. Windows has
no daemon fixture. No new Cargo dependencies; flate2 is already present.

Bounds:256 KiB encoded/compressed body, 16 KiB line, 256 points, 64 tags/fields, 1024 byte names/
org/bucket/token; 64 KiB response, 32 KiB/64 response headers, 128 byte code/1024 byte message;
10s whole exchange, one in-flight write, 32 queued response events/handler actions, 8 follow-up
depth. Commands remain independent of parked handlers and active IO; another write while
busy is rejected atomically. The logical local address is0.0.0.0:0, because TCP opens only
for a write. Standard shared memory refreshes per event; HTTP failures raise typed events
and keep the logical session open. Malformed/oversized responses or transport errors close
the session without retries. Disconnect/removal drops the live HTTP driver/socket future.
