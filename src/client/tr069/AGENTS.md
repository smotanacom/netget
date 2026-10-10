# TR-069 (CWMP) device

`remote_addr` is the ACS URL (a bare `host:port` becomes `http://host:port/`). The device is
described by `serial_number`, `manufacturer`, `oui`, `product_class` (its DeviceId) and `root`
(`Device` or `InternetGatewayDevice`). Its data model is whatever the handler answers.

On connect a session opens for `events` (default `1 BOOT`). A session is POSTs with the ACS's
cookies carried by hand: Inform (with `<root>.ManagementServer.ConnectionRequestURL` and any
parameters the model adds), then empty or answering POSTs until the ACS answers 204.

A **connection-request listener** (`connection_request_listen`, default `127.0.0.1:0`; its URL
is what `connect` returns and what the Inform reports) turns any HTTP GET into a
`6 CONNECTION REQUEST` session. No authentication is checked.

## Events and actions

- `tr069_rpc {method, arguments}` for every RPC the ACS sends. The handler answers it with
  `tr069_parameter_values {parameters, types?}` (GetParameterValues),
  `tr069_parameter_names {parameters: [{name, writable}]}` (GetParameterNames),
  `tr069_done {status?, instance_number?}` (Set/Add/Delete/Reboot/FactoryReset and others), or
  `tr069_fault {code 9000-9899, message}`. An answer of the wrong kind is refused and the RPC
  keeps waiting; with no answer in `ANSWER_TIMEOUT` (60 s) it is fault 9002. GetRPCMethods is
  answered by Rust.
- `tr069_session {ok, events, rpcs, error?}` when a session ends.
- `tr069_inform {events, parameters?}` asks for a session (queued if one is running).

At most `MAX_PENDING_SESSIONS` wait; a session carries at most `MAX_SESSION_RPCS`; a chain
stops after `MAX_FOLLOWUP_DEPTH` (8). Envelopes from the ACS are capped at 1 MiB.
