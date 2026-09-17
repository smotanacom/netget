# XML-RPC Server Implementation

XML-RPC over HTTP POST. The LLM (or a script/static handler) implements every method;
there is no method registry and no response cache in Rust.

**State**: Experimental — LLM-authored, not human-reviewed. The parser and the fault
serializer were rewritten and verified with `curl`.
**Port**: any (no default privileged port). **Stack**: `ETH>IP>TCP>HTTP>XMLRPC`.
**Spec**: http://xmlrpc.com/spec.md

## Library choices

- **quick-xml** v0.37 — parsing and writing. Note: it does **not** expand DTD entities,
  so billion-laughs and XXE are not reachable (both return a `-32700` fault).
- **hyper** v1 — HTTP/1.1
- **base64** — `<base64>` decode/encode

## What the model sees

**Event**: `xmlrpc_method_call`, one per request.

| Field | Notes |
|---|---|
| `method_name` | non-empty; a request without one is rejected before any model call |
| `params` | array of parameters, **typed** |

Type mapping into event JSON:

| XML-RPC | Event JSON |
|---|---|
| `<int>` / `<i4>` | number (i32) |
| `<i8>` | number (i64) |
| `<boolean>` | `true` / `false` |
| `<string>`, or untyped text in `<value>` | string |
| `<double>` | number (non-finite is rejected) |
| `<dateTime.iso8601>` | string, unvalidated |
| `<base64>` | `{"xmlrpc_type":"base64","byte_length":N,"text":<string or null>}` |
| `<array>` | array |
| `<struct>` | object |
| `<nil/>` | `null` |

`<base64>` is deliberately **not** handed over as a base64 string: the project rule
forbids encoded blobs in event data, since a model cannot usefully read or write them.

### The parser was rewritten

Everything below was broken and is fixed; each is verified with `curl` against a static
handler that echoes `{{event.params}}`:

| Input | Was | Now |
|---|---|---|
| `add(<int>5</int>, <int>3</int>)` | `["5","3"]` | `[5,3]` |
| `[<int>1</int>,<int>2</int>,<int>3</int>]` | `[[3]]` — only the last element, the rest leaking into the next parameter | `[[1,2,3]]` |
| `f(<string></string>, <int>7</int>)` | `[7]` — the empty parameter vanished, shifting positions | `["",7]` |
| `<html><body>hi</body></html>` | method `""`, 0 params, one wasted model call | `-32700` fault |
| `<methodCall><methodName>f</methodName>` (truncated) | method `f`, treated as complete | `-32700` fault |

The old parser pushed `XmlRpcValue::String(text)` for every text node and never looked at
the type element, so six of the ten `XmlRpcValue` variants were unreachable on input.
`Int`, `I8`, `Boolean`, `Double`, `DateTime` and `Base64` now all decode, and a malformed
one (`<int>abc</int>`) is a fault rather than a silent string.

Nesting is capped at `MAX_VALUE_DEPTH` (64) and the request body at
`MAX_REQUEST_BODY_BYTES` (4 MiB) — neither had a limit.

The nesting cap is applied on **`<value>`, `<array>` and `<struct>` alike**. It used to be on
`<value>` only, and `<array>` / `<struct>` pushed a container with no check at all — a
well-formed `<array><array><array>…` is 7 bytes a level, so a body at the 4 MiB cap pushed
about 600 000 frames. The parser is iterative, so this was allocation rather than a stack
overflow, but it is allocation a peer chooses and nothing needs.

### XML safety: entity expansion and recursion

- **Entity expansion (billion laughs) and XXE are not reachable.** `quick-xml` does not process
  DTDs at all: an internal subset is skipped and any entity outside the five predefined ones
  makes `unescape()` return `EscapeError::UnrecognizedSymbol`, which this parser propagates as
  a `-32700` fault. There is no external-entity resolver to point anywhere.
- **The parser is iterative, so nesting cannot overflow the stack.** `parse_method_call` is one
  `read_event_into` loop over explicit `Vec` stacks; the depth cap bounds heap growth, not
  stack depth.
- **`xmlrpc_value_to_json` (in `actions.rs`) *is* recursive** over the parsed value, but the
  depth cap above is what bounds it, so it can only be reached 64 frames deep.

Note the **client** side is a different story and is *not* safe: `src/client/xmlrpc/` uses the
`xmlrpc` crate, whose parser recurses without a bound. See `src/client/xmlrpc/CLAUDE.md`.

## Actions

All sync; there are no async actions (XML-RPC is strictly request/response).

| Action | Parameters |
|---|---|
| `xmlrpc_success_response` | `value_type` (required: `int`/`i4`, `i8`, `boolean`, `string`, `double`, `array`, `struct`, `nil`), `value` (required) |
| `xmlrpc_fault_response` | `fault_code` (int), `fault_string` (required) |
| `xmlrpc_list_methods_response` | `methods` (array of strings) |
| `xmlrpc_method_help_response` | `help_text` (string) |
| `xmlrpc_method_signature_response` | `signatures` (array of arrays of type-name strings) |

Every declared field is read by the executor; there are no dead actions and no
undeclared executor branches. The last three are convenience wrappers over
`xmlrpc_success_response` for the `system.*` introspection methods.

Executor behaviour that changed:

- `value_type: "int"` with a value outside i32 is now an error naming `i8`, instead of
  silently wrapping (5000000000 used to go out as 705032704). Same for `fault_code`.
- **`fault_code` accepts the quoted form and no longer defaults silently.** It was
  `.and_then(|v| v.as_i64()).unwrap_or(-32603)`, so `"fault_code": "-32601"` — the quoted form
  models routinely produce, and the form every other numeric field here already accepts through
  `as_integer` — reached the wire as `-32603`: the model said "no such method" and the caller
  was told netget had broken. A present-but-unparseable value is now an error; only an absent
  one defaults to `-32603`, which is the honest reading of "the handler did not say".
- non-finite `double` is rejected instead of emitting `NaN`/`inf`, which is not a valid
  `<double>`.
- `methods` and `signatures` entries of the wrong shape are now errors. They used to be
  dropped by `filter_map`, so a flat `["int","int"]` signature produced an empty array
  with no error, and the log reported the pre-filter count.

## Response generation

- Success responses go through `quick_xml::Writer`, which escapes text.
- **Faults are now escaped too.** `generate_fault` interpolated its message raw with
  `format!`, so a `fault_string` containing `<`, `>` or `&` — "Unknown method: `<foo>`" is
  entirely plausible from a model — produced a document no client could parse. The same
  path carries `quick-xml` and LLM error text. Verified: a fault quoting `a<b&c` now
  parses as valid XML.
- The first `ActionResult::Output` from any action is sent, unwrapping
  `ActionResult::Multiple` (a nested `Output` used to fall through to "no response
  generated"). If nothing produced XML, the client gets a `-32603` fault rather than an
  empty body.
- `Content-Type` is `text/xml; charset=utf-8`. Without the charset, RFC 3023 makes
  `text/xml` default to US-ASCII, so strict clients mis-decoded non-ASCII strings.

## Architecture

- `spawn_with_llm_actions` binds with `?` and registers the accept-loop `JoinHandle`
  exactly once via `AppState::register_server_task`.
- One tokio task per TCP connection; hyper handles framing, `Content-Length` and
  keep-alive. `service_fn` guarantees exactly one response per request on the right
  connection, so correlation is structural — there is no request id to echo.
- Connections are marked closed on exit.

## Not implemented / known gaps

- **`system.multicall`** — no action, no handling. Earlier docs claimed support.
- **No authentication, no rate limiting, no path routing** — every path is an endpoint.
- **Non-POST** returns HTTP 200 with a fault body rather than `405`, so the port looks
  like a working web endpoint to a scanner. (`tests/server/xmlrpc/test.rs` asserts the
  200 behaviour, so changing it means changing that test.)
- **No `Content-Type` validation** on requests.
- **No per-request timeout** — a slow body holds a connection task.
- **Request charset** — the body is read as UTF-8 (`from_utf8_lossy`), so a legal
  `encoding="ISO-8859-1"` document is mangled.
- **Connection byte/packet counters are never updated**, so stats read zero.
- Per-connection tasks are untracked, so `stop_server` does not abort in-flight requests.

## Example

Static handler, no model call:

```json
{"type": "open_server", "port": 8080, "base_stack": "xmlrpc",
 "event_handlers": [{"event_pattern": "xmlrpc_method_call",
   "handler": {"type": "static", "actions": [
     {"type": "xmlrpc_success_response", "value_type": "string", "value": "pong"}]}}]}
```

LLM mode:

```
listen on port 8080 via xmlrpc.
Implement add(a,b) -> int and greet(name) -> string.
For anything else return fault -32601 "Method not found".
```

Because parameters now arrive typed, a script handler can do arithmetic directly:

```python
respond([{'type': 'xmlrpc_success_response', 'value_type': 'int',
          'value': event['params'][0] + event['params'][1]}])
```

That script was shipped as the protocol's script-mode startup example while the parser
was still producing strings, so it concatenated `"5"+"3"` into `"53"` and then failed the
integer conversion, returning `-32603`.

## References

- [XML-RPC Specification](http://xmlrpc.com/spec.md)
- [quick-xml](https://docs.rs/quick-xml/)
- Testing notes: `tests/server/xmlrpc/CLAUDE.md`

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a task and an `AppState` entry forever,
pre-authentication, and a hundred of them was a free denial of service on a server that would
happily accept a hundred more. It now declares both halves; the constants and the argument for
each live beside them in `src/server/xmlrpc/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | HTTP is client-speaks-first, so a peer that has completed the handshake and sent no byte has asked nothing and negotiated nothing — the state carries no protocol yet, which is why this number is the same across netget's HTTP family. Apache's `mod_reqtimeout` gives the request header 20s and nginx's `client_header_timeout` 60s. Enforced with `TcpStream::peek` before the socket reaches hyper, so the request line is still there afterwards. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 60s | A backstop rather than a keep-alive allowance: XML-RPC's canonical client, Python's `xmlrpc.client.ServerProxy`, opens a connection per call and closes it, so there is no legitimate long idle window to protect. 60s is an order of magnitude past a pooled transport's round trip and well short of letting an abandoned connection sit for minutes. |
| `MAX_CONNECTIONS` | 256 | The shared default. Each admitted connection may buffer one body of up to 4 MiB, well inside the ~1 GiB ceiling netget's HTTP family is held to; a protocol declares a smaller number only when its per-connection cost is larger. Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After`**, written straight onto the socket — the peer has sent no request line for hyper to answer — and logged `decision=fail_closed_connection_cap`. Fixed bytes, so nothing derived from an error can reach the wire. |

**The deadline covers the read and nothing else.** hyper owns every read once `serve_connection`
starts, and it keeps polling the connection for more input *while a request is being answered* —
so a deadline on those reads would be wrong here, not merely awkward. The idle bound is a
watchdog over `ConnectionActivity` instead, which reports a connection with work in flight as not
idle at all. The model round-trip, and an event a `manual` rule parked for a human
(`src/state/intercepts.rs`, 300s by default), are therefore outside every deadline by
construction: an answer that takes minutes can never close the connection it is an answer for.
That is the `.connectionless()` lesson in the project `CLAUDE.md` read in reverse — TFTP evicted
live transfers because "idle" was measured wrongly.

**hyper's own `header_read_timeout` is not this bound.** Its 30-second default is inert unless
`http1::Builder::timer` is also set, which nothing here does: hyper downgrades a defaulted
duration to `None` when no timer is present and applies no deadline at all. That is why the
`peek` is not redundant.

`tests/server/xmlrpc/connection_bounds_test.rs` drives all three from the wire, with three
sockets on one server whose only rule is `*` → `manual`: a silent peer must be closed after the
first-byte bound, a peer that sends a request line and then stalls (slowloris) after the idle
bound, and a peer whose request is parked for a human must **not** be closed at all. The shared
driver and the removal-verification notes are in `tests/helpers/http_bounds.rs`.
`tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound disappears.
