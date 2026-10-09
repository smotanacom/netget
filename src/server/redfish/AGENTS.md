# Redfish service — Experimental, DMTF Redfish (DSP0266) over plain HTTP

hyper HTTP/1.1. `model.rs` (shared with the client) holds the envelope rules: paths under
`/redfish/v1` normalised (no `..`, `//` or controls), `@odata.type` as `#Namespace.vX_Y_Z.Type` or
`#XCollection.XCollection`, Base-registry (`Base.1.19.0`) error bodies, Task resources.

Rust serves, without the handler:
- `/redfish` (`{"v1": ...}`), the service root, `/redfish/v1/odata` and `$metadata` (references to
  the DMTF schemas for what it serves) — unauthenticated. `OData-Version: 4.0` on every answer;
  another requested version is a 412.
- SessionService: `POST .../Sessions` raises `redfish_login`; an explicit
  `redfish_login_accept` creates a session (201, `X-Auth-Token`, `Location`, Session resource),
  anything else — reject, silence, failure — is 401 `NoValidSession` with
  `WWW-Authenticate: Basic`. Sessions expire after `session_timeout_secs` (1800) of inactivity;
  `GET`/`DELETE` on them; 64 at most. HTTP Basic goes through the same login event, and an
  accepted credential is remembered (keyed hash) for the session timeout. `auth: none` turns
  authentication off.
- TaskService: a `redfish_task` answer creates a task (202, `Location` = task monitor,
  `Retry-After`); the monitor answers 202 while it runs and the operation's own answer (the
  task's `result`, or 204) once `complete_after_secs` (2) pass; `Tasks/{id}` shows its state and
  messages. 256 tasks, oldest evicted.

Everything else raises `redfish_request` with the method, `kind` (read, update, create, action,
delete), path, body, action name and resource for action POSTs. Answers: `redfish_resource`
(checked: `@odata.id` is the path — filled when absent — type, Id and Name, collection Members of
links with Rust's count; a created member must sit directly under the collection, sent 201 with
`Location`), `redfish_no_content` (204, not for reads), `redfish_task`, `redfish_error` (a named
Base message with its status). No answer, an invalid one or a backend failure is 500
`GeneralError` / 503 `ServiceTemporarilyUnavailable` with a category message. PATCH and POST need
`application/json` (415); PUT and other methods are 405 `OperationNotAllowed`.

Not implemented: TLS (put it in front), `$expand`/`$select`/`$filter`, ETag enforcement, event
subscriptions, SSE, privileges per role. No storage: resources come from the handler.
