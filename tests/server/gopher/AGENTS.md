# Gopher E2E Testing

`tests/server/gopher/e2e_test.rs`. **Five tests, 10 mocked LLM calls.**

## What the Beta rating rests on

`test_gopher_menu_with_real_curl` and `test_gopher_document_with_real_curl` drive
the real `curl(1)` binary. curl ships gopher support (`curl --version` lists it in
`Protocols:`) and accepts an arbitrary port, so `curl gopher://127.0.0.1:PORT/…`
runs unprivileged against a test server on an ephemeral port. That is a genuine
third-party client: a raw socket only proves bytes arrived, whereas curl fetching
the URL and exiting 0 proves the framing, the CRLF endings and the **close** are
all acceptable to an independent implementation.

**Both hard-fail when curl is missing or lacks gopher support. Neither skips.**
`require_curl_with_gopher` returns `Err` — it does not print `SKIP` and return
`Ok`. The root `CLAUDE.md` lists four protocols (`kubernetes`, `oci_registry`,
`maven`, `websocket`) held back from Beta for exactly that: a skip-when-missing
gate is a silent pass on any runner without the binary, and a maturity rating
resting on a silent pass rests on nothing. It also checks the `Protocols:` line
for `gopher`, because a curl built without it would fail with a confusing
"unsupported protocol" rather than saying what is wrong.

## Measured facts about curl's gopher support

These were probed against a throwaway Python server before the tests were
written, not assumed. Each one shapes what the tests can assert:

| Behaviour | Consequence for the tests |
|---|---|
| curl **strips the item-type character** from the URL path — `/1/menu` and `/0/menu` both send `/menu`; `/` sends an empty selector and `/1/` sends `/` | curl can never exercise anything type-dependent on the *request* side. The type in a Gopher URL is the caller's expectation about the reply; the server never sees it |
| curl does **no Gopher-level parsing** — the reply reaches stdout verbatim, terminating `.` line included, doubled leading dots **not** undone | the terminator and the periodating are asserted as literal bytes in curl's stdout |
| a **type-3 error item is still exit 0** — curl has no notion of a Gopher error | the error test uses a socket and asserts the exact bytes; asserting on curl's exit status would assert nothing |
| curl **reads until EOF**, exiting 28 if the server never closes | `curl_gopher` passes `--max-time 15` and its error message says what exit 28 means, so a regression that stopped closing reads as a diagnosis rather than a stall |
| `%09` in the URL becomes a real tab | a type-7 search *can* be spelled in a URL, but only a socket can send a query containing further tabs — which is the case the split rule is about |

## The three socket tests, and why each needs a socket

- `test_gopher_search_request_over_socket` — sends `/search\tterm one\ttwo` and
  asserts the split took the **first** tab only, so the second tab belongs to the
  query. Then sends `/search` with no tab and asserts `search_query` is **absent**,
  not empty. One rule with `respond_with_actions_from_event` branching on the
  event, not two rules on `gopher_request` that nothing could tell apart.
  The generator spells the query's own tab as `|TAB|` before putting it in a menu
  `display`, because the server sanitizes tabs out of menu fields (they would forge
  an extra field) — the assertion is that the tab survived the *split*, not how it
  renders.
- `test_gopher_llm_failure_is_a_type_3_category_not_an_error_string` — declares
  **no rule** for `gopher_request`, so the mock's HTTP 500 makes the event's LLM
  call fail for real. Asserts both halves of the repo-wide rule: the peer is
  answered with a well-formed type-3 item rather than a silent close, and that item
  contains none of `LLM`, `ollama`, `http://`, `model`, `retries`, `anyhow`. The
  unmatched 500s are not counted against any rule, so `verify_mocks` still holds.
- `test_gopher_error_item_and_connection_logging` — asserts the type-3 item byte
  for byte (`3<msg>\t\terror.host\t1\r\n.\r\n`) and waits for the connection log
  line.

## The read-to-EOF helper is itself an assertion

`gopher_request` reads with `read_to_end`, not a fixed byte count. That is
deliberate: RFC 1436 has the server close after one reply, and waiting for the EOF
*is* the check that it did. If the server ever stopped closing, the helper would
block and the test would fail on its 15s timeout instead of quietly passing on a
partial read. The `expect` message says so.

## LLM Call Budget

| Test | Calls |
|---|---|
| `test_gopher_menu_with_real_curl` | 1 startup + 1 request = 2 |
| `test_gopher_document_with_real_curl` | 1 + 1 = 2 |
| `test_gopher_search_request_over_socket` | 1 + 2 = 3 |
| `test_gopher_llm_failure_…` | 1 (+ uncounted failing retries) |
| `test_gopher_error_item_and_connection_logging` | 1 + 1 = 2 |

Every test calls `wait_for_mocks(30)` and then `verify_mocks()`.

## Test Execution

```bash
./cargo-isolated.sh test --no-default-features --features gopher \
    --test server -- --test-threads=100 gopher
```

Runtime ~1s wall for the five tests at 100 threads.

## Privacy

Everything binds 127.0.0.1 on an ephemeral port. curl is pointed at
`gopher://127.0.0.1:PORT` — an IP literal, so no resolver is involved and nothing
leaves the host. No external network access, no real gopher servers.

## Not covered

- Gopher+ and `gophers` (TLS) — the server implements neither.
- Binary items (`9`, `g`, `I`) beyond appearing in a menu; there is no action that
  serves binary content.
- Any client other than curl. A period-correct client (`lynx`, `gopher(1)`) would
  be stronger evidence for the *menu* semantics specifically, since curl treats a
  menu as an opaque byte stream and never renders it as links. What curl proves is
  the framing and the close, not that a browsing client follows the items.
- The 8 KB selector-length refusal.
