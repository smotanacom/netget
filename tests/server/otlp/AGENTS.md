# OTLP receiver evidence

The 24 existing HTTP checks remain part of the suite: independent otel-cli/telemetrygen,
mocked verdicts, JSON/protobuf summaries, unknown field/encoding/routing behavior, raw and
inflated 4 MiB boundaries, recursion bombs, empty exports, no-verdict/backend failures,
partial-count clamping, retry statuses and connection/idle/first-byte bounds. Their source
files and expected behavior remain unchanged except exposing the independent tool runner
to the added gRPC peer test. The expanded protocol remains Experimental.
Final local evidence: all thirty receiver checks passed at 100 test threads with no failed
or ignored checks, together with all nine exporter checks.

`grpc_test.rs` adds generated-client checks for all 3 signal service methods, gzip, partial
success, mapped refusal status and no-verdict fail-closed. Exactly 4 MiB requests succeed;
4MiB+1 plain/gzip requests fail RESOURCE_EXHAUSTED without reaching an event handler,
and a later valid export on the same channel succeeds. Prefix u32::MAX, an extra framed
message and an empty body are refused before decoding/model dispatch.

Ownership tests park a manual handler, exercise a smaller client RPC deadline, remove the
receiver, observe the client's failed call, and require intercept removal. The admission
test occupies 64 RPC slots on 4 connections, rejects the 65th, cancels one request, then proves
its slot is reusable and removal cancels all calls. Test-spawned work is owned by JoinSet.

Independent gRPC peers are mandatory: otel-cli sends/read-acknowledges a span and reports
PERMISSION_DENIED when refused; telemetrygen sends metrics and logs, and semantic access
logs prove the peer's names, counts and log body reached the handler. Local pins are
Homebrew otel-cli 0.4.5 and telemetrygen 0.161.0 (Go 1.27.1). Missing binaries fail, never skip.
The existing real-client HTTP suite continues exercising those same independent exporters.

Run `--no-default-features --features otlp --test server -- otlp:: --test-threads=100`
through the programme serialized Cargo wrapper. `tests/vendored_tonic_patch_test.rs` is a
separate three-check wire/provenance suite for exact 4 MiB/+1 plain/gzip requests and responses,
run with feature grpc. No local unit test is placed in src/.

## Preserved HTTP evidence and local setup

### Files

| File | What it proves | Model calls |
|---|---|---|
| `common.rs` | helpers: in-process receiver, a raw HTTP/1.1 client (exact status, headers, body; de-chunks), gzip, protobuf and OTLP/JSON trace builders | — |
| `real_client_test.rs` | `otel-cli span --protocol http/protobuf --fail` exits 0 only because a script accepted exactly its span name and service (refusing anything else with 403); refused, it exits non-zero naming 403; a mocked model sees `otel-cli`'s span by name and service; `telemetrygen metrics --metrics 3` reaches the model as summaries totalling exactly 3 data points from service `billing`, and `telemetrygen logs` as bodies `payment declined`, whose refusal (400) `telemetrygen` reports. Fails, never skips, without the binaries. | 2 + 1 + 2..n (mocked cases) |
| `e2e_test.rs` | mocked model: a JSON trace export accepted (`200 {}`), a gzip protobuf metrics export partly accepted (a decoded `partial_success` of 2 data points), a JSON logs export refused 429 with `Retry-After: 30` and `{"code": 8, …}`; and NetGet's own 404, 405 (`Allow: POST`), 415 for a content type and for a content encoding, 400 for bad gzip and bad protobuf (`INVALID_ARGUMENT`) — none costing a model call | 4 |
| `codec_test.rs` | the summary of each signal in both encodings (either key style, integers as strings, status by name or number, bytes described not shown), the summary's bounds, decode errors, a 10,000-deep attribute in protobuf (prost's recursion limit) and JSON (serde_json's), the gzip bound at exactly the cap and one byte over, and every response body decoded back with the crate's own types | 0 |
| `connection_bounds_test.rs` | the shared hyper-family checks (`tests/helpers/http_bounds.rs`: first byte, idle, parked, cap); a protobuf export of exactly 4 MiB accepted and one byte more refused 413 with no handler; a gzip body inflating to exactly 4 MiB accepted and one inflating one byte past refused 413 before decoding | 0 (+1 per helper) |
| `llm_failure_test.rs` | dead backend → 500 (or 503 + `Retry-After: 5`) with a fixed `Status` message in the request's own encoding, no leaked error text, `decision=fail_closed_llm_error`; no verdict → 500 + `model_silent`; a partial success clamped to the spans sent; a model 403 sent without `Retry-After` + `model_reject` | 0 |
| `answer_with_test.rs` | the hint names the three verdicts, the count and unit per signal, and the retry statuses; examples are placeholders | 0 |

### How each guard was shown to matter

Removed together in one mutated build, then restored:

| Guard removed | Test that failed |
|---|---|
| the body cap (→ `usize::MAX`) | `a_body_of_exactly_the_cap…` (200 for 4 MiB + 1) |
| the cap after inflation | `a_gzip_body_is_held_to_the_same_cap…` |
| the 500 for no verdict (→ 200) | `a_handler_with_no_verdict…` |
| the partial-success clamp | `a_partial_success_never_rejects_more…` |
| `Retry-After` only on retryable statuses | `a_model_refusal_is_sent…` |
| the 500 for a backend failure (→ 200) | `a_backend_failure_refuses_the_export…` |
| the content-type check (→ JSON) | `e2e` |
| the connection cap (→ 100 000) | `the_connection_past_the_cap…` |
| first-byte and idle deadlines (→ 3600 s) | `silent_stalled_and_parked_peers…` |

The nesting limits are prost's and serde_json's, not NetGet's, so they cannot be removed from
here; `a_depth_bomb_in_either_encoding_is_refused` shows both come back as decode errors.

### Notes

- `otel-cli` is at `/opt/homebrew/bin` (`brew install otel-cli`); `telemetrygen` at `~/go/bin`
  (`go install …/cmd/telemetrygen@v0.161.0`), which `require_tool` searches. CI's
  `registry-audit` installs otel-cli's release `.deb` and telemetrygen through `go install`, and
  runs `otlp::real_client_test`.
- `otel-cli`'s default `--timeout` is one second; the tests pass 30 s so a mocked model's answer
  is not raced.
- Both exporters send protobuf; the JSON path has no third-party sender here.
