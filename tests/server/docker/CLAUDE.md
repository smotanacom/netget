# Docker Engine API tests

## Strategy

**The real `docker` CLI is the peer**, pointed at NetGet with `-H tcp://127.0.0.1:<port>`. It
negotiates from `/_ping`, decodes every document into the Engine's own Go types and renders
them, so a passing `docker ps -a` proves the short ID, the port column, the name and the status
all came out of our JSON the way the CLI expects.

**The machine's own Docker daemon is never contacted.** `docker()` in `real_client_test.rs`
passes `-H` explicitly, removes `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_TLS_VERIFY` and
`DOCKER_CERT_PATH`, and sets `DOCKER_CONFIG` to a temp dir. Every fixture name carries a
`netget-fixture` marker and row counts are asserted exactly, so output from a real daemon could
not pass.

`require_docker()` **hard-fails** when the CLI is absent. No daemon is needed.

## Files

| File | LLM calls | What it proves |
|---|---|---|
| `real_client_test.rs` | 0 | against one static rule carrying an action per route: `docker version` (our Server section, API ≤ 1.47 negotiated), `ps -a` (exactly two rows; truncated ID, quoted command, `0.0.0.0:18080->80/tcp`, statuses), `images` (tags, 188MB), `inspect` (raw document and a Go template through the CLI's decode), `network ls`, `volume ls`, `info` (a template over eight fields), and `rm` refused as read-only; the shipped script-mode example drives `ps`, `inspect web`, and a 404 that the CLI reports as "no such object" |
| `e2e_test.rs` | 7 | a mocked model branching on `resource`/`id`, driven by the CLI: `ps -a`, the default `images` table, `inspect --format` over `Path`/`Args`, an unknown container (the model's 404, plus the `/info` the CLI asks while falling back) and `create` refused statically; `decision=model_answer`, `model_reject`, `fail_closed_not_implemented` in the log. A second test pins negotiation from the wire with `api_version`/`engine_version` startup parameters: `/_ping` headers, too-new and too-old 400s, 404 for unknown paths |
| `llm_failure_test.rs` | 1 | a backend failure makes `docker ps` fail with `Error response from daemon: netget: request could not be processed` (not an empty list), no leaked error text, `{"message": …}` on the wire, `decision=fail_closed_llm_error` |
| `api_test.rs` | 0 | version-prefix splitting, the routing table, the defaults a minimal container gets, ports/status/timestamps, fourteen refusals, `/version`/`/info` defaults |
| `connection_bounds_test.rs` | 2 | the shared hyper-family checks in `tests/helpers/http_bounds.rs` (cap; silent peer at 30 s, stalled peer at 120 s, parked peer kept) and the 1 MiB body cap (at the cap: 501; one byte over: 413) |

**Total: 10 LLM calls**, all mocked.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features docker --test server -- \
    docker:: --test-threads=100
```

`connection_bounds_test.rs` takes about two minutes (it sits out the 120 s idle bound).

## Not covered from the wire

The overload branch (503 + `Retry-After`): the mock backend cannot be made to saturate the rate
limiter on demand.
