# OCPP central system (CSMS) — Experimental, OCPP-J 1.6 and 2.0.1

WebSocket (tokio-tungstenite) at `ws://<host>:<port>/.../<charge point id>`. The handshake
picks the first of `ocpp_versions` (default 1.6, 2.0.1) the charge point offers in
`Sec-WebSocket-Protocol`; no common subprotocol, or a bad id in the last path segment, refuses
the upgrade (400 / 404). 64 KiB messages and frames.

`frame.rs` (shared with the client): CALL `[2,id,action,{}]`, CALLRESULT `[3,id,{}]`,
CALLERROR `[4,id,code,description,{}]`; ids ≤36 characters without controls, actions ASCII
alphanumeric, payloads objects within a JSON budget; each version's error-code list and
spellings (1.6 `OccurenceConstraintViolation`/`FormationViolation`, 2.0.1
`OccurrenceConstraintViolation`/`FormatViolation`); required fields of the core actions in
both directions (Boot, Heartbeat, Status, Authorize, Start/StopTransaction or
TransactionEvent, MeterValues, remote start/stop, Reset, configuration/variables). Full JSON
schemas are the peer's job, not Rust's.

- A charge-point CALL with a core field missing is answered by Rust with the occurrence
  error; a malformed frame with the formation error (echoing the id when one was readable).
  Otherwise `ocpp_call` asks the handler for `ocpp_call_result` (core response fields checked
  before sending) or `ocpp_call_error` (code checked against the version). A backend failure
  or invalid answer is CALLERROR **InternalError** — never an invented Accepted.
- `ocpp_send_call` (peer action via `send_to_peer` or the dashboard): one outstanding CSMS
  call per connection, Rust-assigned `csms-<n>` ids, `call_timeout_secs` (30) after which it is
  forgotten. The answer raises `ocpp_call_response`; its handler may follow up with another
  call or disconnect. An answer to nothing pending is ignored (OCPP-J §4.1.4). Binary frames
  end the session.

No charging database, no security profiles (put TLS/basic auth in front), no schema engine.
