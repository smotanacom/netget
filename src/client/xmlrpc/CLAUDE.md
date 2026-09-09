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

### ⚠️ Do not point this client at an untrusted XML-RPC server

`xmlrpc` 0.15's `Parser::parse_value` → `parse_value_inner` → `parse_value` recurses **with no
depth counter**, and nothing caps the response body it is fed. `<value><array><data>` is about
twenty bytes per nesting level, so a malicious or compromised server can return a few megabytes
that drive the parser tens of thousands of frames deep. A Rust stack overflow is a `SIGSEGV`
against the guard page, not a panic: `spawn_blocking` cannot contain it and **the whole NetGet
process dies**. It is pre-authentication as far as this client is concerned — the reply to the
very first call is enough.

**This cannot be fixed from inside NetGet.** `Request::call_url` owns both the HTTP fetch and
the parse, and the only other entry point — `Request::call` with a custom `Transport` — takes a
`reqwest` **0.11** `RequestBuilder`, a different major version from the 0.12 this crate depends
on (the same version wall that forces `timeout_secs` to be applied around the whole call). A
real fix means replacing the crate or its parser.

It is recorded rather than half-fixed, the way `nfsserve`'s pre-auth DoS is in the root
CLAUDE.md.

**What *is* bounded** is NetGet's own half: `xmlrpc_value_to_json` and `json_to_xmlrpc_value`
are both recursive and both now carry `MAX_VALUE_DEPTH` (64). The first walks a value that came
off the network, so without a counter it was a second, independent way for a deep reply to kill
the process — it just needed the crate's parser to survive first. Anything deeper than 64 is
reported to the model as `<nesting deeper than 64 levels was not decoded>` rather than walked.

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

### Blocking API Wrapper, and where `timeout_secs` bites

xmlrpc crate is synchronous, so we use:

```rust
tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || {
    request.call_url(&server_url)
})).await
```

This runs the blocking HTTP call on a dedicated thread pool without blocking the async runtime.

The `timeout_secs` startup parameter (default 30) is applied **around the whole call**, not on
the HTTP client, and the reason is a version wall: `xmlrpc::Request::call_url` builds its own
`reqwest::blocking::Client` internally with no way to configure it, and the only alternative —
`Request::call` with a custom `Transport` — takes a `reqwest` **0.11** `RequestBuilder`, a
different major version from the 0.12 this crate depends on.

What that bounds and what it does not: the caller stops waiting after `timeout_secs` and gets
an error, which is what the parameter promises. The blocking thread is **not** cancelled —
`spawn_blocking` cannot be — so an unresponsive server still holds one blocking-pool thread
until its own TCP timeout expires.

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

0. **No protection against a hostile server's reply** — see the warning at the top. This is the
   one that matters.
1. **Synchronous HTTP**: Each method call blocks a thread from the tokio blocking pool
2. **No streaming**: Cannot handle long-running methods with progress updates
3. **No e2e test.** `tests/client/xmlrpc/` holds only `command_channel_test.rs`; there is no
   `e2e_test.rs` and the directory is the only one in this family without one. The injected-
   action path is covered; the LLM-driven `xmlrpc_connected` → call → `xmlrpc_response_received`
   chain is not covered by anything.
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
