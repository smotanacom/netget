# HTTP Client E2E Tests

Four files, declared in `tests/client/http/mod.rs`. Nothing is `#[ignore]`d.

| File | Peer | Tests | LLM calls |
|---|---|---|---|
| `real_server_test.rs` | **nginx** | 1 | 5 |
| `e2e_test.rs` | NetGet's own HTTP server | 2 | 8 (4 + 4, server and client mocks) |
| `command_channel_test.rs` | NetGet's own HTTP server, in-process | 1 | 0 |
| `transport_test.rs` | NetGet's own HTTP and TCP servers, in-process | 5 | 0 |

```bash
./cargo-isolated.sh test --no-default-features --features http,tcp --test client -- http:: --test-threads=100
```

## `real_server_test.rs` — the evidence the rating rests on

NetGet's client is `reqwest` over hyper. The server is nginx, a C implementation with its own
HTTP parser, started per test by `tests/helpers/real_server.rs`: foreground, one worker,
unprivileged, `-p {dir} -e stderr`, with pid, access log and all five temp paths inside its temp
dir, on a probed loopback port, ready when it logs `start worker processes` (printed only after
the listener is bound). HTTP **is** this client's protocol, so nginx is valid evidence for it —
the generic-HTTP exclusion rules out protocols layered on HTTP, not HTTP itself. **It fails,
never skips,** when `nginx` is missing.

### `http_client_follows_the_model_through_nginx` (5 calls)

NetGet starts with `default_headers` `User-Agent: netget-e2e/1`. `http_connected` →
`GET /hello.txt` (a static file). Its 200 (matched on `status_code` and the file's text) →
`POST /echo` with `X-NetGet: model saw <body>` and a 13-byte body, the header built from the
response the model was shown. nginx's `return` echoes the request line and header; that 200
(matched on the echo containing the model's header value) → `GET /missing`. The 404 (matched on
`status_code` 404) → nothing.

Then **nginx's own access log** (`$request|$http_x_netget|$http_user_agent|$content_length|$status`)
must be exactly:

```
GET /hello.txt HTTP/1.1|-|netget-e2e/1|-|200
POST /echo HTTP/1.1|model saw hello from nginx|netget-e2e/1|13|200
GET /missing HTTP/1.1|-|netget-e2e/1|-|404
```

**Condition 4 of the client bar**: every field of that log was chosen by the model, one of them
from a response it was shown. Verified by mutation: dropping the actions in
`notify_response`'s loop fails the test.

Two things worth knowing if you extend it:

- **Rule order matters.** The echo quotes `hello from nginx` back, so the echo rule must come
  before the hello.txt rule — first match wins, and in the other order the hello.txt rule
  answers the echo too and the chain loops until the follow-up depth cap stops it.
- **No sleeps.** The last rule is the 404's response, and nginx writes the access-log line when
  it sends a response, so the log is complete once `wait_for_mocks` returns.

## `e2e_test.rs` — same-project

`test_http_client_get_request` and `test_http_client_lllm_controlled_request` (4 calls each,
across a server mock and a client mock) drive the client against NetGet's own HTTP server.
Circular for client evidence — kept because both sides' mocks are asserted.

## Not covered

HTTPS against a real server, redirects and compressed responses. Chunked responses and the
8 MiB body bound are covered by `transport_test.rs`, against NetGet's own TCP server rather
than a third-party one.

## `command_channel_test.rs`

Covers `AppState::send_to_client` injecting an action into a running http client (the
dashboard's `[ send ]`). **Zero LLM calls**: the client's LLM points at
`http://127.0.0.1:1`, so its connected-event call fails and the loop must tolerate that —
part of what the test verifies. It always `wait_for_client_handle`s before sending, which is
the regression guard for "register the command channel *before* the connected-event LLM
call"; register it after and a client whose connect event parks on a manual rule reads "no
command channel" for the whole park.

Asserts the exact `ClientSendOutcome` variant. A successful request is
`Executed { detail }`, **not** `Sent` — reqwest/h3 own the socket and report no wire byte
count, so a byte count would be invented; the detail carries what actually came back
instead. An unknown action must be `Rejected` (not silently swallowed), and `disconnect`
must be `Disconnected` and leave the client with no command handle.

The peer is a NetGet HTTP server of our own with a `*` static handler, so the assertion that
the injected `GET /dashboard-marker` came back `200` is a real round trip, and the server's
access log is checked for the path.

## `transport_test.rs` — the browser transport, natively

`src/client/http/transport.rs` is what the client uses in the browser build; it compiles
natively so it can be tested without one. Every server answers through a `*` static handler,
so there are no LLM calls. NetGet's HTTP server gives a 200 (body, `status_text`, two headers,
and a POST) and a 404 read as a status; NetGet's **TCP** server writes hand-made HTTP bytes,
because NetGet's HTTP server never chunks: a chunked body reassembled, the same body refused
at a 10-byte bound and accepted at exactly 19, and a server that never answers giving up at the
500 ms deadline. `https://` is refused with `HTTPS_UNSUPPORTED`. The last test is the native
client's own reqwest path through `send_to_client`: a 5-byte body is read, a body one byte over
`MAX_RESPONSE_BODY_BYTES` fails the request. Each bound was verified by removal — the body
bound, the deadline and the reqwest chunk check each turn their test red.
