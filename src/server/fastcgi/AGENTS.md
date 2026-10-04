# FastCGI responder — Experimental, FastCGI 1.0, Responder role

A FastCGI application on TCP behind a web server (nginx's `fastcgi_pass`, Apache's
`mod_proxy_fcgi`). `record.rs` (shared with the client) is a hand-written codec: 8-byte
headers (version 1 only), padding, name-value pairs with 1- and 4-byte lengths (lengths checked
against the stream before slicing), streams split at 65 528 bytes and ended by an empty record,
CGI responses built and parsed per RFC 3875 §6.

- Management (request id 0): GET_VALUES answers FCGI_MAX_CONNS / FCGI_MAX_REQS (the connection
  cap) and FCGI_MPXS_CONNS 0, omitting unknown names; any other type → UNKNOWN_TYPE.
- One request at a time per connection: a second BEGIN_REQUEST while one is open gets
  END_REQUEST CANT_MPX_CONN. Authorizer and Filter get UNKNOWN_ROLE. Records for other ids are
  ignored; application-only types from the web server end the connection.
- PARAMS (≤64 KiB, else 431) then STDIN (≤1 MiB, else 413); when STDIN ends, `fastcgi_request`
  carries the method, URI, script name, path info, query, content type, HTTP_* headers, all
  params and the body (text, or hex with `body_encoding`). ABORT_REQUEST before that ends the
  request with REQUEST_COMPLETE and no event.
- `fastcgi_respond` (status, headers checked as HTTP tokens without controls, body utf8|hex,
  optional stderr line) becomes STDOUT records (`Status:` written by Rust), STDERR, then
  END_REQUEST app status 0. KEEP_CONN is honoured; without it the connection closes.
- No answer, an invalid answer or a backend failure is a 503 (with Retry-After) or 500 with a
  category message and app status 1, logged `fail_closed_*` / `model_silent` — never content the
  handler did not give. A refused connection at the cap is closed (a web server cannot read an
  HTTP 503 on a FastCGI socket). Idle connections close after `idle_timeout_secs` (60).

No storage: every response comes from the handler. No Unix-socket listener.
