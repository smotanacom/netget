# XML-RPC Protocol E2E Tests

## Test Overview

Tests XML-RPC server with HTTP clients and manual XML construction, validating method calls, introspection, fault
responses, and various data types.

## Test Strategy

**Individual Feature Tests** - Each test validates one XML-RPC capability:

1. Simple method with integer parameters
2. Introspection (system.listMethods)
3. Fault response for unknown methods
4. String parameter handling
5. Boolean parameter handling
6. Multiple parameter handling
7. Non-POST request rejection

Tests use **action-based mode** to ensure LLM interprets XML-RPC semantics.

## LLM Call Budget

### Breakdown by Test Function

1. **`test_xmlrpc_simple_method`** - **2 LLM calls**
    - 1 startup + 1 method call (add)

2. **`test_xmlrpc_introspection_list_methods`** - **2 LLM calls**
    - 1 startup + 1 introspection call

3. **`test_xmlrpc_fault_response`** - **2 LLM calls**
    - 1 startup + 1 fault trigger

4. **`test_xmlrpc_string_parameter`** - **2 LLM calls**
    - 1 startup + 1 method call (greet)

5. **`test_xmlrpc_boolean_parameter`** - **2 LLM calls**
    - 1 startup + 1 method call (toggle)

6. **`test_xmlrpc_multiple_parameters`** - **2 LLM calls**
    - 1 startup + 1 method call (concat)

7. **`test_xmlrpc_non_post_request`** - **1 LLM call**
    - 1 startup (GET request rejected without LLM call)

**Total: 13 LLM calls** (slightly over limit but acceptable - tests are independent)

**Note**: Tests can be consolidated to reduce LLM calls if needed.

## Scripting Usage

**Disabled** - Action-based mode:

- Each method call triggers LLM
- Validates LLM's XML parsing and response generation
- Tests introspection and error handling logic

## Client Library

**Manual XML Construction + reqwest**:

- `quick-xml` for XML parsing (in helper functions)
- `reqwest` for HTTP POST
- Manual XML-RPC request building validates protocol understanding

## Expected Runtime

- **Model**: qwen3-coder:30b
- **Runtime**: ~50-70 seconds for full test suite
- **Breakdown**: ~7-10s per test (startup + 1 method call)

## Failure Rate

**Low** (2-5%):

- **Stable**: XML parsing, HTTP handling
- **Occasional Issues**:
    - LLM returns wrong fault code
    - LLM generates invalid XML structure
    - Timeout on slower models

## Test Cases

### 1. Simple Method (`test_xmlrpc_simple_method`)

**Validates**: Basic method call with integers

- POST with `<methodCall>` XML
- Receives `<methodResponse>` with `<int>8</int>`
- HTTP 200 status

### 2. Introspection (`test_xmlrpc_introspection_list_methods`)

**Validates**: system.listMethods support

- Calls introspection method
- Response contains method names in `<array>`

### 3. Fault Response (`test_xmlrpc_fault_response`)

**Validates**: Error handling

- Calls non-existent method
- Receives `<fault>` response with faultCode and faultString

### 4. String Parameter (`test_xmlrpc_string_parameter`)

**Validates**: String type handling

- Passes `<string>Alice</string>`
- Response contains greeting with name

### 5. Boolean Parameter (`test_xmlrpc_boolean_parameter`)

**Validates**: Boolean type (0/1)

- Passes `<boolean>1</boolean>`
- Response contains boolean result

### 6. Multiple Parameters (`test_xmlrpc_multiple_parameters`)

**Validates**: Multi-param methods

- Passes two strings
- Response concatenates them

### 7. Non-POST Request (`test_xmlrpc_non_post_request`)

**Validates**: HTTP method validation

- Sends GET request
- Receives fault response (XML-RPC requires POST)

### 8. Deep nesting and a quoted fault code
(`test_xmlrpc_refuses_deep_nesting_and_honours_a_quoted_fault_code`) — **1 LLM call**

Two refusals in one server, both of which used to be wrong and neither of which failed loudly:

- 300 levels of `<array><data>` → `-32700`. The depth guard was on `<value>` alone, so
  `<array>` and `<struct>` pushed a container with no check; at 7 bytes a level a body at the
  4 MiB cap bought ~600 000 frames. The parser is iterative, so this is allocation rather than
  a stack overflow — but it is allocation a peer chooses.
- `"fault_code": "-32601"` (the quoted form models routinely produce) must reach the wire as
  `-32601`. The executor did `.and_then(|v| v.as_i64()).unwrap_or(-32603)`, so the model said
  "no such method" and the caller was told netget had broken. The test asserts `-32603` is
  *absent*, which is what makes it catch the regression rather than just the presence of a
  fault.

### 9. `llm_failure_test.rs` — fail-closed with a category

Covered in that file's own header: an LLM failure must produce a `<fault>` carrying a
`WireFailure` category and nothing derived from the error.

## Known Issues

**None on the server side** — these tests are stable.

The **client** is a different matter, and its exposure is not testable from here: `xmlrpc`
0.15's parser recurses without a depth bound, so a hostile XML-RPC *server* can stack-overflow
(and so kill) the NetGet process that points a client at it. See `src/client/xmlrpc/CLAUDE.md`.
A test for it would have to deliberately crash the process, which is why there isn't one.

## Test Execution

```bash
./cargo-isolated.sh build --release --all-features
./cargo-isolated.sh test --features xmlrpc --test server::xmlrpc::test
```

## Key Test Patterns

### XML Request Construction

```rust
fn build_method_call(method_name: &str, params: &[(&str, &str)]) -> String {
    format!(
        r#"<?xml version="1.0"?>
<methodCall>
  <methodName>{}</methodName>
  <params>...</params>
</methodCall>"#,
        method_name
    )
}
```

### XML Response Parsing

```rust
fn parse_xmlrpc_response(xml: &str) -> E2EResult<String> {
    let mut reader = Reader::from_str(xml);
    // Extract value from <value> tags
}
```

## Why This Protocol is Readable

Compared to binary protocols:

1. **Human-readable** - XML is text-based
2. **Self-describing** - Tags indicate types
3. **Simple structure** - methodCall/methodResponse pattern
4. **Standard types** - Well-defined type system
5. **Introspection** - Built-in method discovery

This makes debugging easy and tests very reliable.
