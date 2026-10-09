# ICAP server — Experimental, RFC 3507

TCP 1344, persistent connections. Rust owns framing, OPTIONS, preview continuation and
response assembly; the handler decides each REQMOD/RESPMOD. No scanning engine in Rust.

`wire.rs` (shared with the client): ICAP heads (64 KiB, 100 headers, CRLF only, header
names are tokens, values without controls); `Encapsulated` must list non-decreasing offsets
ending in a body or `null-body`, with only header entities before it; embedded HTTP heads are
read by exact length and must end in an empty line; chunked bodies have hex sizes of ≤8
digits, extensions only on the zero chunk (`ieof`), ≤4096 chunks and a 1 MiB total checked
before each allocation.

- `services` (startup) lists `{name, methods}`; OPTIONS answers Methods, ISTag, Preview
  (`preview_bytes`, 1024), Transfer-Preview `*`, Allow 204, Max-Connections, Options-TTL.
  Unknown service → 404, method not offered → 405, other methods → 501, non-1.0 → 505,
  malformed head → 400; after any 4xx/5xx the connection closes (the body may be unread).
- A preview that does not end in `ieof` is always continued (`100 Continue`) so the handler
  decides on the whole body.
- `icap_request` gives method, service, the HTTP request/response heads as structured JSON,
  the body as `body_text` when UTF-8 (else `body_binary` + `body_bytes`), `allow_204`.
- `icap_response` verdicts: `no_modification` → 204 when allowed, else the original echoed
  (REQMOD: req-hdr+req-body; RESPMOD: res-hdr+res-body — RFC 3507 §4.4.1 allows nothing else in
  a RESPMOD response, which c-icap-client caught); `block` → an HTTP 403 page; `modify` → a
  replacement request (REQMOD only) or response, Content-Length recomputed; `error` → ICAP
  400/403/404/500/503. A handler failure or invalid verdict is **ICAP 500**, never a 204 that
  would let unscanned content through.

Not implemented: 206 partial content, ICAP over TLS, OPTIONS bodies.
