# Milter client tests

`chain_handlers()` drives a whole transaction: `milter_negotiated` → `milter_connect`, then each
`milter_reply` that continues leads to the next stage (helo, mail, rcpt, a message with a
Subject). No LLM calls.

- `session_test.rs`: NetGet's own filter — the chain, its end-of-message modifications, a second
  transaction (refused recipient, `milter_abort`, refused sender), local refusals, QUIT; a
  hand-driven filter asking for no HELO, no reply to MAIL and SKIP, whose received command
  sequence is asserted (`CMRTLNBEQ`: no HELO, one body chunk of three); and the follow-up bound,
  measured by waiting for the client's own "chain stopped" line and counting exactly 8 replies.
- `real_server_test.rs`: a **pymilter** filter (`pymilter_filter.py`; Sendmail's libmilter in C)
  and **emersion/go-milter's server** (`peer/`), each driven through the same chain. Each must
  return its own header, recipient and Subject change, reject the spam recipient and answer the
  spammer with 550 5.7.1. The pymilter case asserts the negotiated `["no_body", "no_eoh",
  "no_data", "skip"]` — the steps libmilter asked for, which the client used to refuse.

A RCPT sent outside a transaction (after end of message, before MAIL) is silently ignored by
libmilter — the client then waits for a reply that never comes. The tests open a new
transaction with MAIL first, as an MTA must.

Peers from `tests/server/milter/install_peers.py`.
