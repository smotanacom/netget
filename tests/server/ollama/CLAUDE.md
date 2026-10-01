# Ollama Server Tests

The `server` integration target includes these suites whenever the `ollama` feature is enabled.
They use mock model backends or static handlers; no running Ollama installation or downloaded
model is required. Child processes use isolated temporary directories and random bound ports.

## Coverage

- `e2e_test.rs`: the real NetGet process serves list, generate, chat and an invalid endpoint.
  HTTP requests are made with reqwest, and the model decisions are mocked.
- `real_client_test.rs`: independent `ollama-rs` decodes `/api/tags`, `/api/generate` and
  `/api/chat`; reqwest checks the embeddings decision/refusal and a 9 MiB request refused by
  the 8 MiB body cap. The size test also sends an ordinary request so a blanket refusal fails.
- `connection_bounds_test.rs`: raw sockets exercise the first-byte and idle deadlines,
  validate the connection-cap refusal framing, and hold a refused upload's final MiB back
  until the 413 and EOF arrive. Both Content-Length and chunked requests must allow this tail
  to drain without resetting the client. Oversized requests create no access-log event.
- `refusal_status_test.rs`: model-selected HTTP refusal statuses cannot wrap into success,
  accept out-of-range values or turn an error into a 2xx response.
- `embeddings_test.rs`: explicit vectors, dimensions, validation and the deterministic ramp.
- `src/server/ollama/mod.rs::close_tests`: the post-response drain ends at its byte budget or
  deadline, including a peer that leaves its write half open indefinitely.

The connection-bound tests construct the server in-process through `ServerForm`, with a dead
model endpoint and an empty instruction. They do not consume model calls. E2E mock expectations
are local to each test; there is no fixed suite-wide model-call count.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features ollama --test server -- \
  server::ollama:: --test-threads=32
./cargo-isolated.sh test --no-default-features --features ollama --lib -- \
  server::ollama::close_tests --test-threads=32
```

For upload-race stress, repeat the server suite at `--test-threads=100`. Keep one Rust build
running at a time and use the same compiled binary for all repetitions.

## Limits of the evidence

Ollama remains Beta. The independent client proves the three core endpoints, but no real
client currently drives all four model-management endpoints. NDJSON responses are assembled
whole in memory, and no Authorization header is validated. There is no Ollama-specific pcap
oracle or fuzz target here.
