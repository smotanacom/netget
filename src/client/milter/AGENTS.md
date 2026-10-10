# Milter client (the MTA side)

Hands a mail transaction to a filter the way Postfix or Sendmail would, using the server's codec
(`crate::server::milter::wire`). On connect it negotiates version 6, offering every
modification and **every protocol step it can honour**, and raises `milter_negotiated
{version, actions, protocol}`.

## Actions

`milter_connect {hostname, address, port?}`, `milter_helo {name}`, `milter_mail {sender,
esmtp_args?}`, `milter_rcpt {recipient, esmtp_args?}`, `milter_message {headers, body}` (DATA,
each header, end of headers, the body in 64 KiB chunks, end of message — stopping at the first
answer that is not continue), `milter_abort` and `disconnect` (QUIT). Each stage raises
`milter_reply {stage, decision, text?, modifications, implicit}`; at end of message the
modifications are the filter's add/change header, add/delete recipient, replaced body and
quarantine, in order. Fields are refused locally (one line, no NUL, 4096 bytes; an IP address;
1 MiB body) before anything is written.

## Protocol steps — found by pointing a real filter at it

The first version offered no steps and refused any filter that asked for one. **libmilter asks
for one whenever a filter lacks a callback**: a pymilter filter with no `body` method asks for
`SMFIP_NOBODY`, and `no_eoh`, `no_data` and `skip` came with it. go-milter never asks, so it
alone would have let the defect stand. The client now offers the step-skipping flags
(`no_connect` … `no_body`), the no-reply flags (`no_reply_*`, including `SMFIP_NR_HDR`) and
`SMFIP_SKIP`, and honours each: a left-out stage is not sent, a no-reply stage is sent without
waiting, and SMFIR_SKIP during the body jumps to end of message. Both report `decision:
continue` with `implicit: true`, which is what the protocol means by them. It does not offer
`SMFIP_RCPT_REJ` or `SMFIP_HDR_LEADSPC`; a filter asking for anything unoffered is refused.

## Limits

No macros are sent, so a filter keyed on `{auth_authen}` or `{client_addr}` sees none. A handler
chain stops after `MAX_FOLLOWUP_DEPTH` (8) follow-ups. One filter answer may take 60 s.

## Tests

`tests/client/milter/`: NetGet's own filter, a hand-driven filter asking for shortcuts, a
pymilter (libmilter) filter and go-milter's server. See its AGENTS.md.
