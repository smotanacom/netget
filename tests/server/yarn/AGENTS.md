# YARN server E2E tests

Drives the YARN ResourceManager REST endpoints with `reqwest` (a real, independent HTTP client)
and asserts the decoded JSON matches the documented Hadoop `ResourceManagerRest` envelopes. No
real `yarn` CLI is available on macOS/CI, so this is **shape-conformance against the documented
response bodies**, not validation against a real Hadoop client.

## Mock expectations

Default (mocked) mode, no Ollama. Every test ends with `server.verify_mocks().await?`. Mocks match
on the `yarn_request` event's `operation` field so one server handles several endpoints.

## LLM call budget

- `test_yarn_cluster_info_static_and_metrics`: startup (1) + metrics (1). `/ws/v1/cluster/info` is
  **static** — no LLM call. Total 2.
- `test_yarn_apps_list_and_by_id`: startup (1) + apps (1) + app (1). Total 3.
- `test_yarn_submit_application_accepted`: startup (1) + submit (1). Total 2.
- `llm_failure_test`: startup (1) + one unmatched metrics request that forces a 5xx. Total ~2.

Suite total ~9 LLM calls, under the ~10 budget. Localhost only; never contacts external endpoints.

## What each test validates

- Static info banner (`clusterInfo.state == STARTED`, version present) with no model round-trip.
- `send_yarn_metrics` wraps into `{"clusterMetrics":{...}}` and defaults omitted fields to 0.
- `send_yarn_apps`/`send_yarn_app` wrap into `{"apps":{"app":[...]}}` / `{"app":{...}}`.
- `send_yarn_submit_response` accepted → **202 Accepted** with a `Location` header, empty body.
- `llm_failure_test`: LLM failure → 5xx `RemoteException`, never `200 {"apps":null}`.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features yarn \
    --test server::yarn::e2e_test -- --test-threads=100
```
