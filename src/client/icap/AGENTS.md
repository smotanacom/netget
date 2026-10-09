# ICAP client — Experimental

`icap_request` {method OPTIONS|REQMOD|RESPMOD, service, http_request, http_response,
body_text, preview, allow_204 (default true)} on one persistent connection, one request at a
time. With `preview: N` the first N bytes go with a `Preview` header (ending `0; ieof` when the
whole body fit); a `100 Continue` gets the rest. `icap_response` carries status, reason, ICAP
headers, the adapted HTTP heads as JSON, the body (text or size) and whether the preview was
continued. A 2xx other than 204 without `Encapsulated`, a non-ICAP/1.0 version or malformed
framing ends the session. Injected-action log entries record method and service only. Shares
`src/server/icap/wire.rs` with the server.
