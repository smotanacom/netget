# NETCONF server — Experimental, NETCONF 1.0/1.1 over SSH

`netconf` serves [RFC 6241](https://www.rfc-editor.org/rfc/rfc6241.html) over the SSH
`netconf` subsystem ([RFC 6242](https://www.rfc-editor.org/rfc/rfc6242.html)) on TCP 830.
russh 0.45 carries SSH; NetGet owns everything above it: the `<hello>` exchange, framing,
envelopes, `message-id`/`session-id`, capability rules and kill-session. A handler decides
SSH password acceptance and every RPC's data, acceptance or `rpc-error`. There is **no
datastore in Rust**: get/get-config/edit-config are answered by the handler; persist with
server memory or the SQLite facility if a scenario needs state.

## Files

- `mod.rs` — accept loop (`accept_bounded`, 256 connections), one owned task per SSH
  connection, the russh `Handler`, and the per-channel NETCONF session.
- `owned_stream.rs` — `OwnedStream`: russh spawns its own session driver, so dropping the
  connection task cannot abort it. Every read/write of the stream polls the owner's
  cancellation token first; dropping `Owner` wakes a read parked on a quiet socket and the
  driver exits. No TCP stream is cloned.
- `wire.rs` — RFC 6242 end-of-message (`]]>]]>`) and chunked framing; lengths, chunk count
  (1024) and the 1 MiB message bound are checked before body copies.
- `xml.rs` — bounded flat XML (no DOM recursion): DTDs and entities refused, depth 32,
  8192 nodes, 64 KiB text, 16 namespaces, 32 attributes. `parse_fragment` parses handler XML
  inside a sentinel wrapper so a fragment can never close the envelope it is placed in;
  `render_fragment` re-declares every in-scope binding on top-level elements.
- `rpc.rs` — `<hello>`, `<rpc>`, `<rpc-reply>` for both roles; RFC 6241 Appendix A error
  tags; datastore/capability rules.
- `actions.rs` — `netconf_rpc_reply`, `netconf_auth_decision`, events `netconf_rpc`,
  `netconf_auth`.

## Session

1. SSH password auth raises `netconf_auth`; no decision, a backend failure or a refusal all
   reject (`decision=fail_closed_*` / `model_reject` in the log). Public-key auth is not
   offered.
2. One session channel per connection, then the `netconf` subsystem. The server sends its
   `<hello>` (base:1.0, base:1.1, plus the `capabilities` startup parameter — default
   `:writable-running`) with a positive `session-id`. A client `<hello>` carrying a
   session-id, or sharing no base version, ends the session. base:1.1 on both sides switches
   to chunked framing.
3. `handshake_timeout_secs` (30) bounds TCP accept → completed hello; `idle_timeout_secs`
   (600) bounds waiting for bytes only, so a parked handler never times out a session.
4. Each `<rpc>` must carry `message-id`; every attribute of `<rpc>` is echoed on
   `<rpc-reply>` with its namespace binding.

## What Rust answers without the handler

- `close-session` → `<ok/>`, EOF, CHANNEL_CLOSE, then the connection ends 2 s later unless
  the client hangs up first.
- `kill-session` → ends another live session of this server (its owner token is cancelled);
  its own id or an unknown id → `invalid-value`.
- Refusals with the RFC tag: missing `message-id` (`missing-attribute` with
  `bad-attribute`/`bad-element` in the base namespace), more than one operation
  (`malformed-message`), candidate/startup without the capability, edit-config on running
  without `:writable-running`, `validate` without `:validate`, `commit`/`discard-changes`
  without `:candidate`, xpath filters without `:xpath`, `test-option` without `:validate`,
  `rollback-on-error` without its capability (`invalid-value` / `operation-not-supported`),
  `url`, confirmed commit, `copy-config`, `delete-config` (`operation-not-supported`).
- A message that is not an `<rpc>` at all → `malformed-message` reply without a message-id,
  then the session closes.

Operations outside the base namespace are custom RPCs: the handler sees `input_xml` and may
answer `output_xml`, `ok` or `errors`.

## Handler contract

`netconf_rpc_reply` takes exactly one of `ok: true`, `data_xml` (get/get-config only),
`output_xml` (custom only) or `errors` (1–64 RFC 6241 errors; `error_info_xml` is parsed with
the base namespace as default, so `<session-id>` means the RFC element). A wrong shape,
invalid XML, a missing answer or a backend failure becomes `operation-failed` ("the server
cannot answer this request right now") — the peer gets a category, the log gets the reason
and a `decision=` tag. XML text from the handler is parsed with the wire bounds; it is never
spliced into the envelope.

## Not implemented

copy-config, delete-config, confirmed commit, url, notifications (RFC 5277), call-home,
NETCONF over TLS, YANG validation, NACM, public-key authentication, more than one NETCONF
channel per SSH connection. Experimental; evidence in `tests/server/netconf/AGENTS.md`.
