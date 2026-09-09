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
