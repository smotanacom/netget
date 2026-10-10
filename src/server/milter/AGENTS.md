# Milter server (the filter)

Sendmail's milter protocol, version 6, hand-written over Tokio TCP (`wire.rs` is the codec, shared
with the client). An MTA — Postfix `smtpd_milters`, Sendmail `INPUT_MAIL_FILTER`, OpenDKIM's
miltertest — connects, negotiates, and asks for a decision at each SMTP stage. There is no
IANA port; the operator names one (OpenDKIM's 8891 is only a convention), so the server is in
`NO_WELL_KNOWN_PORT`.

## What the handler sees and decides

- Events: `milter_connect {hostname, address, port}`, `milter_helo {helo}`,
  `milter_mail {sender, esmtp_args}`, `milter_rcpt {recipient, esmtp_args}` and
  `milter_message {sender, recipients, headers, body, body_truncated}`. Each carries the
  `connection` so far and the `macros` the MTA sent (at most 256 kept).
- Decisions: `milter_continue`, `milter_accept`, `milter_reject`, `milter_tempfail`,
  `milter_discard`, `milter_reply {code 4xx/5xx, xcode?, text}` (SMFIR_REPLYCODE; the xcode's
  class must match the code).
- At end of message only, modifications before the decision: `milter_add_header`,
  `milter_change_header {name, index from 1, value}` (an empty value deletes),
  `milter_add_rcpt`, `milter_del_rcpt`, `milter_replace_body`, `milter_quarantine {reason}`.
  One the MTA did not allow in negotiation is logged and **not sent**: an MTA is entitled to
  treat an unnegotiated modification as a protocol error and drop the filter.

Rust answers DATA, end of headers, each header and each body chunk with continue and collects
them, so the handler is asked five times per message, not once per packet. Recipients the
handler refused are not added to the message's `recipients`.

## Failure modes and bounds

- Saying nothing continues (accepts at end of message), logged `decision=model_silent_continue`.
- A handler failure or an invalid reply answers **tempfail** (`fail_closed_llm_error`,
  `fail_closed_invalid_reply`): the sender retries, and the mail is neither passed nor lost.
  Never accept on failure — for a filter that is the fail-open bug.
- Packets above 256 KiB, an unknown command or a malformed CONNECT close the connection; the
  body is collected up to 1 MiB (`body_truncated` past it), 512 headers, 256 macros;
  `idle_timeout_secs` (default 300) between commands, 30 s to finish a packet.
- Not implemented: protocol-step negotiation from the filter side (the server asks for every
  stage), SMFIR_SKIP, progress keepalives, SMFIR_CHGFROM / INSHEADER, symbol lists.

## Tests

`tests/server/milter/`: raw packets (`wire_test.rs`), OpenDKIM's miltertest and
emersion/go-milter's client (`real_client_test.rs`). See its AGENTS.md.
