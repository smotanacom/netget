# Spark server E2E tests

Drives the Spark monitoring REST endpoints with `reqwest` (a real, independent HTTP client) and
asserts the decoded JSON matches the documented Spark monitoring REST shapes — crucially that
success responses are top-level JSON **arrays**. No real Spark client is available on macOS/CI, so
this is **shape-conformance against the documented response bodies**, not real-client validation.

## Mock expectations

Default (mocked) mode, no Ollama. Every test ends with `server.verify_mocks().await?`. Mocks match
on the `spark_request` event's `operation` field so one server handles several endpoints.

## LLM call budget

- `test_spark_version_static_and_applications`: startup (1) + applications (1). `/api/v1/version`
  is **static** — no LLM call. Total 2.
- `test_spark_jobs_stages_executors`: startup (1) + jobs (1) + stages (1) + executors (1). Total 4.
- `llm_failure_test`: startup (1) + one unmatched applications request forcing a 5xx. Total ~2.

Suite total ~8 LLM calls, under the ~10 budget. Localhost only; never contacts external endpoints.

## What each test validates

- Static `/api/v1/version` banner (`{"spark": "..."}`) with no model round-trip.
- `/applications`, `/jobs`, `/stages`, `/executors` are top-level JSON **arrays** (asserted with
  `is_array()`), matching Spark's monitoring API.
- `llm_failure_test`: LLM failure → 5xx JSON *object* with an `error` field, never `200 []`.
  It waits for the socket by retrying the request rather than sleeping a fixed 500ms, and
  the client carries its own 25s timeout so a server that answers *nothing* fails the test
  instead of hanging it.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features spark \
    --test server -- server::spark::e2e_test --test-threads=100
```

## The `decision=` tag

`llm_failure_test.rs` asserts more than the 5xx: it requires a log line naming
`/api/v1/applications` and carrying `decision=fail_closed_`, and requires that **no** line
carries `decision=model_`. The status alone cannot carry this, because `send_spark_error` lets
the model choose a 500 or a 503 of its own — so without the tag a backend outage and a model
that decided to answer 500 are the same event in the log.

Spark's other two tagged outcomes are `decision=model_answer` / `decision=model_reject` (split
on whether the model's chosen status is < 400) and `decision=model_silent` (the model answered
with no `spark_response`). The two no-LLM paths, `/api/v1/version` and the 404, log
`decision=static_answer` / `decision=unknown_endpoint` at DEBUG, so they stay in `netget.log`
and off the status stream the harness reads.
