# OTLP/HTTP Receiver Implementation

The receiver side of the OpenTelemetry Protocol over HTTP (OTLP 1.x, "OTLP/HTTP"). The model is
the receiver's judgement: from a summary of each export it decides to accept it, accept part of
it, or refuse it. NetGet reads and decodes the payload, and encodes every response.

**State**: Beta (see Maturity). **Privilege**: `None` — the well-known port is 4318.
**Stack**: `ETH>IP>TCP>HTTP>OTLP`. **Feature**: `otlp` (`opentelemetry-proto`, `prost`, `flate2`).

## Library choice

- **`opentelemetry-proto` 0.30** (the OpenTelemetry project's generated types) for protobuf, with
  `default-features = false` and only `gen-tonic-messages`, `trace`, `metrics`, `logs`. That
  builds cleanly and pulls four crates into the lock (`opentelemetry`, `opentelemetry_sdk`,
  `opentelemetry-proto`, and `tonic` 0.13 for its codec traits — no transport). The generated
  gRPC client/server modules are behind the `gen-tonic` feature and are not compiled.
- **JSON is not decoded through the crate's serde layer** (`with-serde`). That layer accepts
  64-bit integers only as strings and enums only as numbers, and OTLP/JSON senders differ on
  both; a strict typed decode would refuse exports a collector takes. The body is parsed as a
  `serde_json::Value` and walked by `codec::summarize_json`, accepting lowerCamelCase keys (the
  specification's) and snake_case (the proto's), numbers or strings for integers, and status
  codes by number or name.
- `google.rpc.Status` is a two-field `prost::Message` in `codec.rs` (`RpcStatus`).
- hyper HTTP/1.1, as every hyper-family server here.

## Files

| File | What it holds |
|---|---|
| `mod.rs` | accept loop, the hyper service, `handle_request` (routing, content negotiation, bounded body, gzip, decode, the model's verdict → response) |
| `codec.rs` | `Signal`, `Encoding`, `gunzip_bounded`, `Summary` and the two decoders, `export_response`, `RpcStatus`/`status_body`, the HTTP→gRPC code map |
| `actions.rs` | the `Protocol`/`Server` impls, three actions, `Verdict`, the `otlp_export` event and `answer_with` |

## Spec subset

| Request | Answered by | Response |
|---|---|---|
| `POST /v1/traces`, `/v1/metrics`, `/v1/logs` with `application/x-protobuf` or `application/json` (parameters ignored), optionally `Content-Encoding: gzip` | the model → `otlp_export` | its verdict, in the request's content type |
| another path | NetGet | 404 text |
| another method on a signal path | NetGet | 405, `Allow: POST` |
| another `Content-Type` | NetGet | 415 text (no encoding to answer in) |
| another `Content-Encoding` | NetGet | 415 `Status` |
| body past the cap, raw or inflated | NetGet | 413 `Status`, `decision=fail_closed_too_large` |
| invalid gzip | NetGet | 400 `Status`, `decision=fail_closed_bad_gzip` |
| a payload that does not decode (bad protobuf, bad JSON, a list that is not a list, nesting past prost's 100 or serde_json's 128 levels) | NetGet | 400 `Status` (`INVALID_ARGUMENT`), `decision=fail_closed_bad_payload` |

Unknown JSON fields are ignored, as the specification requires. An empty export is valid and is
asked about like any other.

**The event** (`otlp_export`) carries: `signal`, `encoding`, `compressed`, `body_bytes`,
`resource_count`, `service_name` (the first resource's `service.name`), `service_names` when more
than one, `resource_attributes` (the first resource's, up to 20, values as text cut at 200 bytes),
and per signal `span_count`/`error_span_count`/`span_names` (first 10), `metric_count`/
`data_point_count`/`metric_names` (first 10), or `log_record_count`/`error_log_count`/
`log_bodies` (first 5, cut at 200 bytes); plus `answer_with`. Attribute arrays, maps and bytes
are described (`<array of 3>`, `<5 bytes>`), never expanded — nothing in either decoder's walk
recurses, and no raw bytes reach the model. Trace and span ids are not shown.

## What the model sees and controls

| Action | Response |
|---|---|
| `accept_otlp` | 200, the empty `Export*ServiceResponse` (`{}` in JSON) |
| `accept_otlp_partially {rejected, error_message}` | 200 with `partial_success` — `rejected_spans`, `rejected_data_points` or `rejected_log_records` (clamped to the export's count), and the message |
| `reject_otlp {code, message, retry_after_secs?}` | `code` ∈ 400, 401, 403, 413, 429, 500, 502, 503, 504, with a `google.rpc.Status` (gRPC code per the specification's mapping); `Retry-After` only on the retryable 429/502/503/504 |

The first verdict in the model's order is the one sent. `answer_with` names the three verdicts,
the item count and unit, and which statuses make the client retry; the examples are
placeholders (`<why>`).

## Failure behaviour

`FailureMode::Answers`. Never an invented 200 — a success tells the exporter to discard its copy.

| Situation | Wire | Log |
|---|---|---|
| backend saturated | 503, `Retry-After: 5`, `Status{14, "netget: backend at capacity, retry later"}` | `decision=fail_closed_llm_error category=Overloaded` |
| backend failed otherwise | 500, `Status{13, "netget: request could not be processed"}` | `decision=fail_closed_llm_error category=Unavailable` |
| no verdict from the model | 500, `Status{13, "netget: the receiver reached no decision on this export"}` | `decision=model_silent` |
| accept / partial | 200 | `decision=model_answer` |
| refusal | its status | `decision=model_reject status=…` |

No error text reaches the peer.

## Bounds

| Bound | Value | Why |
|---|---|---|
| `MAX_BODY_BYTES` (= `max_inbound_bytes`) | 4 MiB | gRPC's default message limit, which OTLP receivers inherit and SDK exporters batch under. Applied by `http_body_util::Limited` to what arrives, and **again** by `gunzip_bounded` to what it inflates to (it reads at most limit + 1 bytes of output), so a few-kilobyte gzip bomb is refused 413 rather than inflated. |
| nesting | 100 (prost) / 128 (serde_json) | the libraries' own recursion limits; both depth bombs are tested to come back as decode errors. NetGet's own walks do not recurse. |
| `FIRST_BYTE_READ_TIMEOUT` | 30 s | enforced with `peek` before hyper sees the socket. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 120 s | silence between requests; a request in flight is not idle. |
| `MAX_CONNECTIONS` | 256 | the peer past it gets `503` + `Retry-After: 5`, which exporters retry. |

## Wireshark

`tshark -G protocols` has no OTLP dissector; the table maps `otlp` to `http`. The `protobuf`
dissector reads the bodies only with the OpenTelemetry `.proto` files on its search path.

## Maturity

Beta. Evidence: `tests/server/otlp/real_client_test.rs` drives
`otel-cli` (equinix-labs' Go OTLP client over the OpenTelemetry Go protobuf bindings) and the
Collector project's `telemetrygen` (the OpenTelemetry Go SDK's OTLP/HTTP exporters) — two
independent exporters, neither linked by NetGet — and fails, never skips, without them. `otel-cli
--fail` exits by our status code, so its exit is a reading of the response; `telemetrygen` reports
a refusal. CI's `registry-audit` installs both. Promoted after the whole suite (24 tests, with
`nsq`'s 35 alongside) passed three consecutive runs at `--test-threads=100` and
`scripts/beta_evidence_table.py --check` stayed green with `otel-cli` and `telemetrygen` as the
peers.

Not covered: OTLP/gRPC (port 4317), TLS, profiles, and the JSON encoding from a third-party
exporter (both clients send protobuf; the JSON path is covered by NetGet's own tests only).
