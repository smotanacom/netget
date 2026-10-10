# Milter server tests

`FILTER_SCRIPT` (in `wire_test.rs`) is the policy everything here runs: evil.example rejected at
connect, a sender containing "spammer" answered `550 5.7.1`, `<spam@…>` recipients rejected,
and a message tagged `X-NetGet: checked`, its Subject prefixed `[netget] ` and `<audit@…>`
added — unless its Subject is "quarantine me". No LLM calls: it is a script handler.

- `wire_test.rs`: raw packets built with NetGet's codec — negotiation, every stage, end of
  message with only the modifications the MTA allowed, then with all of them; the bounds (an
  oversized length and an unknown command close the connection) and the fail-closed path (no
  handler: tempfail at connect and at end of message).
- `real_client_test.rs`: OpenDKIM's **miltertest** (C) runs `miltertest.lua`, which checks
  every reply and `mt.eom_check`s the added header, changed Subject, added recipient and the
  quarantine of a second message on the same connection; exit status and "all checks passed"
  are the verdict. **emersion/go-milter's client** (`tests/client/milter/peer`) runs a clean
  message, a spam recipient and a refused sender, and its own decoding of the modifications is
  asserted exactly.

Both fail rather than skip when absent. Peers: `python3 install_peers.py <root>` (apt
`miltertest python3-milter`, Go 1.24+), which prints `NETGET_MILTERTEST`,
`NETGET_MILTER_PYTHON` and `NETGET_MILTER_GO_PEER`. CI: the `milter-pairs` job in
`.github/workflows/protocol-pairs.yml`.

go-milter's client wraps the sender and recipients in `<…>` itself, and its server strips them —
the peer passes bare addresses for that reason.
