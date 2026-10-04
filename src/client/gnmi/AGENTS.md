# gNMI client — owned RPCs and subscriptions

The native `gnmi` feature uses the same pinned OpenConfig schema and bounded
codec as the receiver. Actions are `gnmi_capabilities`, `gnmi_get`, `gnmi_set`,
`gnmi_subscribe`, `gnmi_poll`, `gnmi_cancel`, `wait_for_more` and `disconnect`.
Calls use fresh positive u32 `call_id` values. Injected actions report queued
execution; actual wire results arrive in typed response/update/sync/ended
events. Unsupported nested request fields are refused, including SAMPLE and
other unimplemented subscription controls.

Paths and values are typed objects, with 64-bit integers and nanosecond
timestamps as decimal strings. Each unary response contains exactly one
message and then EOF. Set verifies that every operation is acknowledged in
transaction order. Get notifications must match the requested encoding.
Subscribe tracks the requested mode, initial/POLL snapshot sync, updates_only,
and final status. False, missing or duplicate sync and out-of-snapshot updates
are errors; `response_count` includes only validated messages. POLL is allowed
only after the previous snapshot synced. Cancellation remains responsive
while other calls and model handlers are parked.

`use_tls` defaults to false. Verified TLS uses webpki roots and optional bounded
regular-file `ca_file`; `server_name` overrides the certificate name. Name and
certificate failures reject startup. There is no insecure verifier, automatic
reconnect, client certificate authentication or arbitrary metadata action.
Bracketed IPv6 authorities are converted to literal socket/name addresses.

`connect_timeout_secs` defaults to 10, range 1..60; it includes DNS, socket,
CA loading and TLS/HTTP2 connection. `rpc_timeout_secs` defaults to 300, range
1..3600. The local whole-RPC timer owns wire futures and bounded event-queue
waits; peer grpc-timeout is additionally sent. Model callbacks after event
enqueue belong to the client owner, rather than the completed RPC's timer.
`idle_timeout_secs` defaults to 120, range 1..3600, and closes only when calls,
handlers and pending events are absent.

One registered owner contains at most 16 calls, 16 handler futures, 16 pending
events, a 16-entry wire event channel and a 16-entry command channel. Each
subscription input channel has one entry; each connection admits 256 fresh
call IDs and each subscription 256 request/response messages. Automatic action
batches have at most 16 actions and depth four. Dropping the owner cancels all
futures and shuts down its owned socket; disconnect/remove clears its command
handle. There is no retained device/configuration store.

All wire, typed value, JSON, recursion and collection bounds are shared with
the receiver and apply before model events. Maturity is Experimental; see the
receiver document for selected forms and exclusions and the client test
document for exact peer, TLS, deadline and cancellation coverage.
