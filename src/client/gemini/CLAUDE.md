# Gemini client

Uses the existing `gemini` feature, adding the already optional webpki-roots dependency
for certificate validation. Canonical name Gemini, TLS/TCP/1965. Specification:
https://geminiprotocol.net/docs/protocol-specification.gmi (0.24.1).

`gemini_request` takes an absolute URL and optional input text. URLs cannot contain controls,
userinfo, fragments or another scheme. Rust encodes input as a UTF-8 percent-encoded query
(spaces become %20, plus becomes %2B). The request URL must match the configured TLS server
name and remote port. To follow a redirect to another host, open another client explicitly.
There is no automatic input submission, redirect following, retry or credential reuse.

`gemini_connected` occurs after the first verified TLS handshake. `gemini_response` exposes
status, meta and kind (input, success, redirect, temporary_failure, permanent_failure or
certificate_required). Successful text responses include MIME type and text; text/gemini
also includes structured headings, links resolved against the requested URL, lists, quotes,
text and preformatted blocks. Input includes sensitivity, redirects include the resolved
target/permanence, and 44 includes retry_after_secs. Unknown second status digits follow
the defined first-digit class. Binary MIME types and non-UTF8/US-ASCII charsets fail explicitly.

Each transport carries exactly one request. The initial connection is retained until the
first action; later actions create fresh TCP/TLS connections. The logical client remains
available for more actions after each response. All events pass through call_llm_for_client
and support static/script/manual handlers, memory and LLM budgets. The tracked event task
cannot block the tracked socket task. Queues hold 16 entries. The command handle exists
before the initial TLS handshake; disconnect/removal cancels a stalled handshake or complete
transaction. Ordinary commands during a pending transaction receive an explicit busy rejection.

TLS 1.2/1.3 uses rustls's explicit ring provider and validates certificates/hostnames.
`server_name` overrides the hostname inferred from remote_addr for SNI, verification and
URL endpoint validation. `custom_ca_cert_pem` replaces Mozilla trust roots with 1..32 supplied
PEM certificates (maximum 256 KiB); use the known capsule certificate to trust a self-signed
capsule. There is no insecure bypass or hidden persistent TOFU store. Client certificate
authentication is not implemented;60..69 responses are surfaced for operator handling.

Connect and initial TLS handshake each have 30 s deadlines. Every subsequent transaction's
30 s deadline covers connect, handshake, write, header and complete body. Header bound 1029
bytes includes the two status digits, space, 1024-byte META and CRLF; URL bound 1024 bytes;
body bound 1 MiB; gemtext bound 8192 lines. Strict CRLF and UTF-8 decoding, before model dispatch.
No capsule content storage exists. The implementation remains Experimental.

The pair test also covers an existing server correction: Gemini's file-backed certificate
startup now selects a process TLS provider before entering the shared certificate loader,
so library callers with both rustls providers compiled no longer panic.
