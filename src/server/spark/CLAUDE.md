# Spark — Apache Spark monitoring REST / history API server

The LLM roleplays a Spark application's control plane and invents the applications, jobs, stages
and executors it reports (the Kubernetes/OCI "model is the control plane" pattern). NetGet owns the
HTTP stack (hyper v1, `http1::serve_connection`); the model only decides state.

**Port**: 4040 (live driver UI/REST) or 18080 (History Server). **Privilege**: `None` (both ports
unprivileged — do NOT declare `PrivilegedPort`). **Stack**: `ETH>IP>TCP>HTTP>SPARK`.
**State**: Experimental. No storage.

## Endpoints

| Method + path | Operation | Handled by |
|---|---|---|
| `GET /api/v1/version` | version | **static** (banner, no LLM) |
| `GET /api/v1/applications` | applications | LLM → `send_spark_applications` |
| `GET /api/v1/applications/{id}` | application | LLM → `send_spark_applications` (one element) |
| `GET /api/v1/applications/{id}/jobs` | jobs | LLM → `send_spark_jobs` |
| `GET /api/v1/applications/{id}/stages` | stages | LLM → `send_spark_stages` |
| `GET /api/v1/applications/{id}/executors` | executors | LLM → `send_spark_executors` |
| other `/api/v1/applications/{id}/...` | application | LLM (answer or 404 via `send_spark_error`) |
| anything else | unknown | **static** 404 plain text (no LLM) |

**Static vs LLM split**: `/api/v1/version` is a mechanical version string, answered directly in
`mod.rs` from the `spark_version` startup param — no model round-trip. Unrecognised paths get a
static plain-text 404. Everything describing (invented) application state is LLM-driven.

## Response shapes

Spark's monitoring API returns **top-level JSON arrays**, not objects. `execute_action` emits the
array supplied by the model directly as the body (`Content-Type: application/json`):
`[{application}]`, `[{job}]`, `[{stage}]`, `[{executor}]`. `/api/v1/version` is the one object,
`{"spark":"<version>"}`. Errors are plain text (`Content-Type: text/plain`), matching Spark
(e.g. `unknown app: app-1`) — except the fail-closed path, see below.

## Events / actions

One event type, `spark_request`, emitted for every LLM-handled request (version/unknown
short-circuit before it). It declares the full action set via `.with_actions(...)`, and every
action is reachable, so nothing is declared-but-unemitted.

Actions: `send_spark_applications`, `send_spark_jobs`, `send_spark_stages`,
`send_spark_executors`, `send_spark_error`. All parameters are structured JSON arrays — never raw
bytes or base64. `spark_response` is the internal `ActionResult::Custom` name, not an emittable
action.

## Fail-closed

On an LLM error the server answers **503** (overload/retryable) or **500** with a JSON error object
`{"error": "...", "status": ...}`. When the model succeeds but emits no `spark_response`, the
server answers **500** rather than a bare `[]` with 200 — an empty array is a valid "no
applications/jobs" result and a client cannot tell it from a backend that never ran. The failure
path (a JSON *object* with an `error` field) is structurally distinct from any success array.

## Startup parameters

`spark_version` (optional, default `3.5.1`) — the version in the static `/api/v1/version` banner,
actually read in `spawn()`. `send_first` is not declared.

## Limitations

Monitoring API only (the tractable, testable core). No standalone-master submission endpoint
(`POST /v1/submissions/create`), no SQL/streaming/environment/storage sub-resources, no
event-log download. HTTP/1.1 only. State is virtual and lives only in the model's context.

**No authentication of any kind**, and nothing about the requester reaches the model: the
`spark_request` event carries `method`, `path`, `operation` and `app_id` and nothing else, so
there is no header, credential or peer address for it to decide on. Every request is answered
unconditionally. (The event's log template used to interpolate `{client_ip}` and
`{client_port}`, which the event has never carried — a missing field renders as the empty
string, so every INFO line read `Spark  GET /api/v1/applications`.)

**Request bodies are capped** at `MAX_REQUEST_BYTES` (64 KiB) through
`http_body_util::Limited`. The monitoring API is read-only and the body is used for nothing but
a `trace!` line, but it was read with an unbounded `collect()`, which made an unauthenticated
POST of any size a way to grow the process. An over-cap body is dropped, not refused — nothing
downstream reads it and every endpoint here is a GET.

**Maturity stays `Experimental`, deliberately.** The tests drive `reqwest`, a generic HTTP
client: it proves the server answers HTTP and that the bodies are the JSON arrays Spark's API
documents, not that a real Spark client or History Server UI accepts them. That is the
generic-HTTP-client exclusion the root `CLAUDE.md` lists, and it is the whole distance between
this rating and Beta.

## References

- Spark Monitoring REST API:
  https://spark.apache.org/docs/latest/monitoring.html#rest-api
