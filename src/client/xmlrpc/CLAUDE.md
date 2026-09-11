# XML-RPC Client Implementation

## Overview

XML-RPC client implementation for calling remote procedure calls over HTTP using XML encoding.

## Library Choices

### xmlrpc crate (v0.15.1)

**Why xmlrpc:**

- Mature, actively maintained library
- Simple API for method calls: `Request::new("method").arg(param1).arg(param2)`
- Built on reqwest for HTTP transport
- Full XML-RPC type support (int, bool, string, double, datetime, base64, array, struct, nil)
- Synchronous API (wrapped in tokio::task::spawn_blocking for async compatibility)

**Alternatives considered:**

- `dxr_client`: More modern (Dec 2024), but repository archived and moved to Codeberg
- `xml-rpc`: Less mature alternative

### A hostile server's reply, and why NetGet does its own HTTP

`xmlrpc` 0.15's `Parser::parse_value` → `parse_value_inner` → `parse_value` recurses **with no
depth counter**, and the crate caps neither the response body nor the nesting.
`<value><array><data>` is about twenty bytes per nesting level, so a malicious or compromised
server can return a megabyte that drives the parser tens of thousands of frames deep. A Rust
stack overflow is a `SIGSEGV` against the guard page, not a panic: `spawn_blocking` cannot
contain it, `catch_unwind` cannot see it, and **the whole NetGet process dies** — every other
server and client in it. It is pre-authentication as far as this client is concerned: the reply
to the very first call is enough.

**The seam is `Transport`, and this file used to deny that it existed.** It said the fix was
impossible because "`Request::call` with a custom `Transport` takes a `reqwest` 0.11
`RequestBuilder`". That is one *provided* implementation of the trait, not its signature:
`xmlrpc::Transport` is public, with an associated `Stream: Read`, and anything may implement
it. Reading a provided impl as the interface is what left the bug open for months.

So `perform_call` no longer calls `Request::call_url`. It:

1. serialises the request with `Request::write_as_xml`,
2. POSTs it with NetGet's own reqwest 0.12 client — built once per (host, timeout) on the
   blocking pool and cached, with the literal-IP resolver bypass applied,
3. reads the body through `response_guard::read_body_capped`, which refuses at
   `MAX_RESPONSE_BYTES` (8 MiB) **as it streams**, so the cap is never exceeded even
   transiently,
4. measures element nesting with `response_guard::scan_depth` (quick-xml, already a dependency
   of this feature) and refuses past `MAX_ELEMENT_DEPTH` (256 elements ≈ 64 XML-RPC value
   levels, matching `MAX_VALUE_DEPTH`),
5. hands the crate a `PrefetchedTransport` over the screened bytes, so `xmlrpc` still does the
   parsing and fault handling, type coverage and error shapes are unchanged.

Refuse, never truncate: a response cut off at the cap parses into something the model would be
told the server said. Each refusal logs `decision=fail_closed_response_too_large`,
`_too_deep` or `_unscannable`. A body quick-xml cannot scan at all is refused rather than
passed through hopefully — a depth we could not measure is not a depth we can certify, and a
response that is not well-formed XML would fail in the crate's parser anyway.

`tests/client/xmlrpc/response_guard_test.rs` drives a hand-written hostile HTTP server that
answers with 50 000 nesting levels. Remove `MAX_ELEMENT_DEPTH` and the test binary does not
fail politely — it aborts with `fatal runtime error: stack overflow` (SIGABRT), which is how
the bound was verified. Its second half points a client at a three-level reply and asserts it
still parses, because a guard that refused everything would pass the first assertion.

**Two things it does *not* bound.** Nesting between 64 and 256 elements is accepted by the scan
and then rendered by `xmlrpc_value_to_json` as `<nesting deeper than 64 levels was not
decoded>`; that is a truncated *rendering* to the model, not a truncated wire read, and it is
the pre-existing `MAX_VALUE_DEPTH` behaviour. And a server that answers slowly but validly is
bounded only by `timeout_secs`.

**`timeout_secs` now actually cancels.** It is applied both on the reqwest client and around
the whole exchange, and the fetch is an ordinary future rather than a `spawn_blocking` thread
that cannot be cancelled — so an unresponsive server no longer holds a blocking-pool thread
until its own TCP timeout. What remains on the blocking pool is the scan and parse, bounded by
`MAX_RESPONSE_BYTES`.

**NetGet's own half** stays bounded independently: `xmlrpc_value_to_json` and
`json_to_xmlrpc_value` are both recursive and both carry `MAX_VALUE_DEPTH` (64). The first
walks a value that came off the network, so without a counter it was a second, independent way
for a deep reply to kill the process — it just needed the crate's parser to survive first.
Neither bound replaces the other.

**Entity expansion is *not* a way in.** `xmlrpc` parses through `xml-rs`, which does not process
DTD internal subsets and rejects any entity outside the five predefined ones, so billion-laughs
and XXE are unreachable. The recursion is the whole of the exposure.

## Architecture

### Connection Model

XML-RPC is **connectionless** - each method call is an independent HTTP POST request:

1. **Initialization**: Store server URL in client protocol_data
2. **Method Calls**: On-demand HTTP requests with XML-encoded parameters
3. **No persistent connection**: Unlike TCP/SSH, no socket kept open
4. **Stateless protocol**: Each call is independent (though LLM has memory)

### LLM Integration Flow

```
User Instruction
    ↓
LLM generates action: call_xmlrpc_method
    ↓
Execute action → Build XML-RPC Request
    ↓
HTTP POST to server (blocking in spawn_blocking)
    ↓
Receive XML response → Parse to xmlrpc::Value
    ↓
Convert to JSON → Event: xmlrpc_response_received
    ↓
Call LLM with result
    ↓
LLM decides next action (another call, disconnect, etc.)
```

### Data Conversion

**JSON → XML-RPC Value:**

- `null` → `String("")` (XML-RPC has no null in spec, though extensions exist)
- `bool` → `Bool`
- `number` (integer) → `Int` (i32) or `Int64` (i64)
- `number` (float) → `Double`
- `string` → `String`
- `array` → `Array`
- `object` → `Struct`

**XML-RPC Value → JSON:**

- `Int`, `Int64` → `number`
- `Bool` → `bool`
- `String` → `string`
- `Double` → `number`
- `DateTime` → `string` (ISO 8601)
- `Base64` → `string` (base64-encoded)
- `Array` → `array`
- `Struct` → `object`
- `Nil` → `null`

## LLM Control Points

### Async Actions (User-triggered)

**call_xmlrpc_method**

- Method name: Any string
- Parameters: Array of mixed types (LLM constructs parameter list)
- Example: `{"type": "call_xmlrpc_method", "method_name": "system.listMethods", "params": []}`

**disconnect**

- No parameters
- Stops client monitoring task

### Sync Actions (Network event responses)

**call_xmlrpc_method**

- Same as async version
- Triggered after receiving a response
- Allows chained method calls

### Events

**xmlrpc_connected**

- Triggered on initialization
- Provides server URL
- LLM can make initial method call

**xmlrpc_response_received**

- Triggered after each method call
- Provides method_name and result (or fault)
- LLM analyzes response and decides next action

## Implementation Details

### Async fetch, blocking parse, and where `timeout_secs` bites

The `xmlrpc` crate is synchronous, but only its *parser* is now used from NetGet. The HTTP is
ours and asynchronous:

```rust
let body = tokio::time::timeout(timeout, fetch_and_screen(&http, &url, request_xml)).await?;
tokio::task::spawn_blocking(move || {
    response_guard::scan_depth(&body)?;
    request.call(response_guard::PrefetchedTransport::new(body))
}).await
```

See the warning near the top for why. What this changes about `timeout_secs` (default 30): it
is applied both on the reqwest client and around the whole exchange, and because the fetch is
an ordinary future rather than a `spawn_blocking` thread, it **cancels** — an unresponsive
server no longer holds a blocking-pool thread until its own TCP timeout. The blocking work that
remains is the depth scan and the parse, both bounded by `MAX_RESPONSE_BYTES` rather than by
whatever the server chose to send.

A related correction: a transport or parse failure is now an `Err`, and only a real `<fault>`
becomes `CallOutcome::Fault`, distinguished with `xmlrpc::Error::fault()`. Every `Err` used to
be reported as a fault, so a refused connection looked to the model like the server saying no.

### Error Handling

XML-RPC supports structured faults:

```xml
<methodResponse>
  <fault>
    <value>
      <struct>
        <member>
          <name>faultCode</name>
          <value><int>4</int></value>
        </member>
        <member>
          <name>faultString</name>
          <value><string>Too many parameters.</string></value>
        </member>
      </struct>
    </value>
  </fault>
</methodResponse>
```

The LLM receives the fault as:

```json
{
  "method_name": "foo",
  "fault": {
    "error": "Too many parameters."
  }
}
```

**One `error` string, not a `code`/`message` pair** — this file claimed the latter, and no
code ever produced it. `xmlrpc::Fault` is rendered with `Display` into a single string, so the
numeric `faultCode` is only present inside that text if the crate chose to include it. A
handler branching on `event['fault']['code']` gets `None` every time.

## Limitations

0. **A hostile server's reply is bounded, not made harmless.** See the warning at the top for
   exactly what `response_guard` refuses and what it does not.
1. **Blocking parse**: the fetch is async; the depth scan and the crate's parser run on the
   tokio blocking pool, bounded by `MAX_RESPONSE_BYTES`
2. **No streaming**: Cannot handle long-running methods with progress updates
3. **No e2e test.** `tests/client/xmlrpc/` holds `command_channel_test.rs` and
   `response_guard_test.rs`; there is no `e2e_test.rs` and the directory is the only one in
   this family without one. The injected-action path is covered, as is a hostile reply; the
   LLM-driven `xmlrpc_connected` → call → `xmlrpc_response_received` chain is not covered by
   anything.
3. **Type limitations**:
    - Null handling varies (converted to empty string for compatibility)
    - Binary data must use Base64 encoding
    - No native support for complex types beyond struct/array
4. **No authentication**: xmlrpc crate doesn't provide built-in HTTP auth (would need custom transport)
5. **No TLS configuration**: Uses reqwest defaults (could add custom transport for cert validation control)

## Common Use Cases

### System Introspection

```json
{
  "type": "call_xmlrpc_method",
  "method_name": "system.listMethods",
  "params": []
}
```

### Simple Calculator

```json
{
  "type": "call_xmlrpc_method",
  "method_name": "examples.getStateName",
  "params": [41]
}
```

### Complex Parameters

```json
{
  "type": "call_xmlrpc_method",
  "method_name": "blogger.newPost",
  "params": [
    "appkey123",
    "blogid456",
    "username",
    "password",
    {
      "title": "My Post",
      "description": "Post content here"
    },
    true
  ]
}
```

## Testing Servers

**Public XML-RPC test servers:**

- http://betty.userland.com/RPC2 (historical test server)
- http://phpxmlrpc.sourceforge.net/server.php (validator)

**LLM can test with:**

- `system.listMethods` - List available methods
- `system.methodSignature` - Get method signature
- `system.methodHelp` - Get method documentation

## Security Considerations

- No authentication mechanism in current implementation
- Sends data in plaintext (use HTTPS URLs for encryption)
- LLM can call any method on the server (instruction should specify allowed methods)
- No rate limiting (LLM makes calls as fast as it decides)

## Future Enhancements

1. **Custom Transport**: Add HTTP Basic Auth support
2. **TLS Configuration**: Certificate validation control
3. **Timeout Control**: Per-call timeout (the `timeout_secs` startup parameter is per-client)
4. **Connection Pooling**: Reuse HTTP connections (reqwest::Client stored in protocol_data)
5. **Multicall Extension**: Batch multiple calls in one request (system.multicall)


## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running XML-RPC client. The handle is
registered **before** the `xmlrpc_connected` LLM call — which this client awaits inline in
`connect()` — because a dashboard-created client defaults to a `*` → manual rule and that
call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s" task is gone: the command loop is now this client's
long-lived task and ends when the client is removed or an injected `disconnect` arrives.
The connected-event handler, the model's follow-up calls and the command loop all go
through one `apply_action`, so the `xmlrpc_call` decoding exists exactly once.

**Outcome semantics — `Executed`, never `Sent`.** The `xmlrpc` crate owns the HTTP socket
(on the blocking pool) and reports no byte count, so `Sent { bytes_sent }` would be
invented. The command loop **awaits** the call and reports
`Executed { detail: "xmlrpc_call 'system.listMethods' with 0 param(s): server returned a result" }`
or `... server returned fault: <faultString>` — a `<fault>` is a real answer from the
server, not a transport failure, and is reported as such rather than as a success. A
transport failure is an `Err`, an unknown action name is `Rejected { error }`, and
`disconnect` is `Disconnected`.

`xmlrpc_response_received` still fires; for an injected call it is raised from its own
registered task (`Dispatch::Deferred`) so a manual rule parking that LLM call cannot wedge
the command loop. `call_method` is unchanged for callers; it is now `perform_call` (network
only) followed by `notify_call_result` (the LLM event plus the model's follow-up calls).
