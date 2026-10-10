# TR-069 (CWMP) auto-configuration server

SOAP 1.1 over HTTP/1.1 (hyper), well-known port 7547 (IANA's cwmp port). `wire.rs` is shared
with the device role: envelopes are read with quick-xml into a bounded element tree
(`MAX_ENVELOPE` 1 MiB, `MAX_DEPTH` 32, `MAX_ELEMENTS`, no DOCTYPE — so no entity expansion)
and turned into JSON shapes; envelopes are written by NetGet with every value XML-escaped.

## Sessions

CWMP sessions are device-initiated and cookie-bound (`netget_cwmp`):

1. The device POSTs Inform. The model sees `tr069_inform {device_id, events, parameters,
   retry_count, remote_addr}` and answers with the RPCs to send (at most `MAX_QUEUED`), or
   `tr069_reject {message}` (fault 8001, no session). InformResponse sets the cookie.
2. The device POSTs empty: the next queued RPC goes out (with a fresh `cwmp:ID`), or 204 ends
   the session.
3. The device POSTs the RPC's response or fault: the model sees `tr069_response {device_id,
   method, ok, result | fault, pending}` and may queue more; then step 2 again.

RPCs the model can queue: `tr069_get_parameter_values {names}`, `tr069_set_parameter_values
{values, types?, parameter_key?}` (JSON values give default xsd types: string, boolean,
unsignedInt, int), `tr069_get_parameter_names {path, next_level?}`, `tr069_add_object
{object}`, `tr069_delete_object {object}`, `tr069_reboot {command_key?}`,
`tr069_factory_reset`. They go in queue order; RPCs queued while answering a response join
the end. A session carries at most `MAX_SESSION_RPCS`.

From the device, TransferComplete and GetRPCMethods are answered by Rust; anything else
in-session that is not a response is fault 8000, and anything but Inform outside a session is
fault 8003. Sessions idle for `session_timeout_secs` (default 60) are forgotten; at most
`MAX_SESSIONS` are open (503 past it).

## Failure

A failed model call on an Inform answers fault 8002 with a category and opens no session; on a
response it ends the session (204) and sends nothing more. Decisions are logged
`decision=model_answer|model_reject|model_silent|fail_closed_*`.

## Not implemented

Device authentication (HTTP Basic/Digest), TLS, connection requests to devices, Download,
Upload, ScheduleInform, GetParameterAttributes/SetParameterAttributes, persistence.
