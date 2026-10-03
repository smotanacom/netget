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
