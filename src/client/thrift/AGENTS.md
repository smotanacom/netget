# Thrift client — Experimental

Uses `src/server/thrift/`'s IDL parser, codecs and JSON mapping. Startup parameters: `idl`,
`service`, `protocol` (binary or compact) and `transport` (framed or buffered). On connect it
reports `thrift_connected` with the service's functions. `thrift_call` {method, args} encodes the
arguments by their declared types (a missing required field or a wrong type is refused before
anything is written), sends the call, waits up to 30 s for the reply with the same sequence id and
reports `thrift_result` with `result`, `exception` {name, type, value} or `application_error`
{type, message}. A oneway call sends and reports nothing. One call is outstanding at a time.
