# Cap'n Proto RPC server

Hand-written, because the `capnp` crate decodes a struct only through generated code or a
schema compiled into the binary, and this server is typed by a schema chosen at startup.

- `layout.rs`: segment framing (64 segments, 4 MiB) and a pointer-following reader that checks
  every target against its segment, follows single and double far pointers, charges every
  struct and list to a traversal budget of four times the message size (so pointers aliasing
  one big text cannot amplify), and stops nesting at 64; a single-segment builder; a deep copy
  used to echo a message back as `unimplemented`.
- `schema.rs`: a CodeGeneratorRequest (`capnp compile -o-`) read with the same reader, schema.capnp
  offsets written out by hand. The JSON mapping: fields by name, enums by enumerant name, a union
  as its one set member, groups as nested objects, Data as `{"$hex": …}`, primitive defaults by
  XOR (an absent field reads as its declared default). Unknown keys and out-of-range numbers are
  refused. Interface and AnyPointer fields are not mapped; generics are not supported.
- `rpc.rs`: rpc.capnp messages (offsets from `capnp compile -ocapnp rpc.capnp`), shared with the
  client. Booleans defaulting to true are stored inverted.

## Startup

`schema`: inline source (what the model can supply; a file id derived from the text is added
when absent), a `.capnp` path (with the file's directory as the import path) — both compiled
with the `capnp` tool, a declared dependency — or a compiled file, which needs no tool. `interface`: the
bootstrap interface, by short or full name. Superclass methods are served too.

## What the handler decides

`capnp_call {interface, method, params, results_shape}` → `capnp_return {results}` (checked
against the results struct; a field it lacks or a wrong type is never sent) or
`capnp_exception {reason, kind}`. A method id the interface lacks is `unimplemented` with no
handler call; a call on anything but the bootstrap capability (its import, or the promised
answer of a bootstrap question, pipelined) is a `failed` exception; any other message type is
echoed back as `unimplemented`.

## Failure modes and bounds

A handler failure, silence or an answer that does not fit is an exception carrying
`WireFailure`'s text (`overloaded` when the model is overloaded), never results. A malformed
message, or one past the size, segment, nesting or traversal bounds, closes the connection
(each bound verified by raising it: the test then gets an answer). 30 s per message once
started; `idle_timeout_secs` (default 300) between messages; 64 open bootstrap questions.
No well-known port.
