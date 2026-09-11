# openapi — spec-driven HTTP API server

Loads an OpenAPI 3.x specification, matches requests against its paths, and asks the model to
produce the response body. `DevelopmentState::Experimental`, group `AI & API`, keywords
`openapi` / `rest` / `rest api` / `api` / `swagger`. Feature
`openapi = ["http", "dep:openapi-rs", "dep:matchit"]` — note it pulls in `http`, so
`http_common` is available here (unlike the other Authentication-group protocols).

Not an auth protocol, but it sits next to them because it is the usual way to stand up the
resource server that consumes the tokens `oauth2` / `openid` hand out. It has no authentication
layer of its own: `security` schemes in the spec are not enforced, and an `Authorization`
header just arrives in the event for the model to read.

## Files

| File | Contents |
|---|---|
| `mod.rs` | `OpenApiServer::spawn_with_llm_actions`, `OpenApiState`, `build_router`, route matching, `handle_llm_response` |
| `actions.rs` | `OpenApiProtocol` (`Protocol` + `Server`), three async and three sync actions, `OPENAPI_REQUEST_EVENT` |

## Libraries

- **openapi-rs** (git, `baerwang/openapi-rs`) parses the YAML/JSON spec.
- **matchit** builds a `Router<RouteMetadata>` keyed `METHOD:PATH`, giving path-template
  matching (`/users/{id}`) with parameter extraction.
- **hyper** serves HTTP/1.1.

No OpenAPI *server framework*: none of them let a model author the response, and the point here
is that it can also deliberately violate the spec.

## Two modes

**With a spec** (`startup_params.spec`) — the router is built at spawn time. A path or method
the spec does not contain is answered immediately with 404/405 and **no LLM call**. A matched
request reaches the model with only the relevant operation attached, not the whole document.

**Without a spec** — every request goes to the model, which can load a spec later with
`reload_spec`.

`llm_on_invalid` (default `false`) flips the first mode: set it with `configure_error_handling`
to have the model author the 404/405/400 bodies too, which is what a honeypot wants.

## Startup parameters

`spec` (string, optional) — inline YAML or JSON. **That is the only one.**

`spec_file` used to be declared here and documented on `spawn_with_llm_actions`, but nothing
ever read it: spawn looks only at `spec` and, when `startup_params` is present without it,
returns the error "OpenAPI server requires 'spec' parameter". A caller passing only `spec_file`
got a hard startup failure from a parameter the protocol advertised. It has been removed rather
than implemented — the server deliberately wants inline content, and giving model-supplied
input an arbitrary file read is not a trade worth making. Read the file and pass the contents.

Note the asymmetry: `startup_params` absent entirely ⇒ dynamic mode; `startup_params` present
but without `spec` ⇒ startup error.

## Event and actions

One event, `openapi_request`, carrying `method`, `path`, `uri`, `headers`, `body`, `spec_info`
and — when a route matched — `matched_route` (`operation_id`, `path_template`, `path_params`,
and the full `operation` spec).

`call_llm` advertises `event.event_type.actions`, **not** `get_sync_actions()`.
`OPENAPI_REQUEST_EVENT` had no `.with_actions(...)`, so the model was told "No specific actions
available for this event" and had no way to answer a request at all; it now carries
`.with_actions(OpenApiProtocol.get_sync_actions())`. `get_event_types()` is likewise
implemented, so `get_protocol_docs` and the script-template prompt can see the event id.

| Action | Kind | Effect |
|---|---|---|
| `send_openapi_response` | sync | status, headers, body — the normal answer |
| `send_validation_error` | sync | status + `{"error": message}` |
| `provide_openapi_spec` | sync | loads a spec mid-request (surfaces as `load_openapi_spec`) |
| `reload_spec` | async | replaces the spec and rebuilds the router |
| `get_spec_info` | async | `NoAction` — logs only, returns nothing to the caller |
| `configure_error_handling` | async | sets `llm_on_invalid` |

**Action name vs result name.** The model emits `provide_openapi_spec`; the executor returns an
`ActionResult::Custom` *named* `load_openapi_spec`, which is what `handle_llm_response` matches
on. `load_openapi_spec` is not an action the model can invoke — an earlier version of this file
showed it as one, and a model copying that example would have had it rejected as unknown.

`send_openapi_response` also accepts `spec_compliant` (bool, default true). The executor reads
it but it was undeclared, so the model could not know it existed; it is declared now. It only
affects the log line — the response is sent either way. That is the switch for "answer 201
where the spec says 200" scenarios that test client error handling.

## Nothing here may panic

`handle_llm_response` routes through `http_common::handler::build_safe_response`. Both
`status_code` and every response header are model output: the previous
`Response::builder().status(status_code)…body(..).unwrap()` panicked inside the connection task
on a status outside 100–599, or on a header value containing CR/LF (a response-splitting
attempt). Out-of-range statuses become 500 and bad headers are dropped individually.

## Failure handling — fail closed, category only

A model answer, a model refusal, a model silence and a backend error are four different things
and stay distinguishable in the log. Only the first is a success on the wire:

| Case | Response | Log |
|---|---|---|
| Model returns `send_openapi_response` | as instructed | `decision=model_response` |
| Model returns `send_validation_error` | its status (400 if it gave none) | `decision=model_reject` |
| Model returns no response action | **500** + `{"error":"Internal Server Error","message":"request could not be processed"}` | `decision=fail_closed_no_action` |
| LLM call errors — overloaded | **503** + `Retry-After: 1` + `"backend at capacity, retry later"` | `decision=fail_closed_llm_error category=overloaded` |
| LLM call errors — anything else | **500** + `"request could not be processed"` | `decision=fail_closed_llm_error category=unavailable` |

A silent model used to be answered with **200** and a body reading "OpenAPI server received
request but LLM did not generate a response" — a no-answer that read on the wire as a success,
and a body naming netget's own LLM machinery to a stranger. Both are gone: the peer now gets a
category from `crate::utils::WireFailure` (`&'static str`, so nothing derived from the error can
reach it) and the error itself goes to the log and the TUI status stream via `Log`.

Overload is split off from a plain failure deliberately: it is transient, and 503 +
`Retry-After` makes a client back off instead of recording a permanent fault.

`tests/server/openapi/fail_closed_test.rs` pins the no-action case at 500 and scans the body
for internal tokens.

## Storage

None, per the project rule. The parsed spec and router are configuration, not data: there is no
resource store behind the paths. `GET /todos` returns whatever the model says, and a `POST`
that "creates" something creates nothing. If a scenario needs the second request to see the
first one's effect, the model must keep it in server memory.

## Bounds

**Request bodies are capped at 8 MiB** and refused with 413. `Incoming` has no default limit,
and `security` schemes are parsed but never enforced (below), so every route is reachable
pre-auth — a single POST with an endless chunked body was enough to walk the process out of
memory. `http_body_util::Limited` errors as soon as the cap is passed, so an oversized upload
costs at most the cap.

The old error arm was worse than merely unbounded: it swallowed the read failure into
`Bytes::new()` and carried on, so the model was shown a request with **no** body and answered
it as though the peer had sent none. That is the truncated-body trap, and it is why the cap
returns a distinct 413 rather than an empty body. The cap is small because the body goes into
the `openapi_request` event and from there into an LLM prompt.

One narrowing cast survived the pass that fixed `send_openapi_response` and
`send_validation_error`: the legacy `ActionResult::Output` branch read `status` with `as u16`
without going through either executor, so it was the one remaining route by which `65736`
could reach `build_safe_response` as a perfectly valid 200. It is range-checked now, and an
unusable value answers 500 rather than the 200 the old default supplied.

## Not implemented

Request-body and parameter schema validation, response validation, content negotiation,
`multipart/form-data`, and any authentication — `security` schemes in the spec are parsed but
never enforced.

Note that `validate_request()` exists, is called on every matched route when `llm_on_invalid`
is false, and **returns `Ok(())` unconditionally** — its body is a TODO listing the four
checks it would do. A reader of the call site would reasonably conclude requests are validated
against the schema; nothing is. The `immediate_400` path it guards is therefore unreachable.

## Examples

```json
{"type": "open_server", "port": 3000, "base_stack": "openapi",
 "startup_params": {"spec": "openapi: 3.1.0\ninfo:\n  title: TODO API\n  version: 1.0.0\npaths:\n  /todos:\n    get:\n      operationId: listTodos\n      responses:\n        '200':\n          description: List of todos"},
 "instruction": "Return three plausible todo items for listTodos."}
```

Deterministic equivalent — no LLM call per request:

```json
"event_handlers": [{"event_pattern": "openapi_request", "handler": {"type": "static",
  "actions": [{"type": "send_openapi_response", "status_code": 200,
    "headers": {"content-type": "application/json"}, "body": "{\"status\": \"healthy\"}"}]}}]
```

## Tests

`tests/server/openapi/` exists and is declared in `tests/server/mod.rs`. See
`tests/server/openapi/CLAUDE.md`.

## References

[OpenAPI 3.1](https://spec.openapis.org/oas/v3.1.0.html), [matchit](https://docs.rs/matchit/),
[openapi-rs](https://github.com/baerwang/openapi-rs).
