# EPP server — Experimental

An EPP registry (RFC 5730) over TLS with RFC 5734 framing (a 4-byte length including itself),
HTTPS-style: TLS by default with a published self-signed certificate (`protocol_data.certificate_pem`),
`tls_cert_file`/`tls_key_file`, or `tls: false` for plain TCP. Default port 700 (privileged).

Rust owns, in `mod.rs` and `wire.rs`:
- The greeting (svID from `server_id`, svDate, version 1.0, lang en, the domain, host and contact
  object URIs, a DCP) on connect and on `<hello/>`.
- Sessions: object commands before login are 2002; login checks version 1.0 (2100), refuses an
  unserved objURI (2307) and, when `clients` is configured, the password (2200; the third failure
  answers 2501 and closes); a second login is 2002; logout answers 1500 and closes. Without
  `clients` every login is accepted. The password never reaches the handler.
- Poll answers 1300 (no messages); there is no message queue.
- Parsing (`xml.rs`: quick-xml, namespace-aware, no DOCTYPE, 32 levels, 4096 elements): 2001 for
  malformed XML, a DOCTYPE or a non-EPP root, 2000 for an unknown command, 2307 for an unserved
  object namespace, 2003 for a missing required element, 2005 for a bad transfer op or clTRID,
  2101 for a command a mapping lacks. Frames over 256 KiB are answered 2500 and closed before
  they are read.
- svTRID per response; the clTRID is echoed.

The handler gets `epp_command` (command, object, the command's fields, client_id, cl_trid) and
answers with `epp_check_result`, `epp_info`, `epp_created`, `epp_renewed`,
`epp_transfer_status` (default 1000, 1001 for a pending request) or `epp_result` (any non-session
code); Rust renders chkData, infData, creData, renData and trnData per object and checks the
answer fits the command. No answer, a failed model or an answer that does not fit is 2400
(`model_silent`, `fail_closed_llm_error`, `fail_closed_invalid_reply`).

Not implemented: extensions (accepted at login, never acted on), poll messages, contact transfer
data beyond the generic trnData, and any storage — the handler is the registry.
