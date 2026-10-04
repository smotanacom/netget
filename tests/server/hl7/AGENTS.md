# HL7 MLLP tests

Peers: `python3 tests/server/hl7/install_peers.py ROOT` (python-hl7 0.4.5, hash-pinned wheel,
owned venv) prints `NETGET_HL7_PYTHON`; `peer.py` drives its public MLLP client and server.

- `peer_test.rs` — python-hl7's MLLP client sends ADT^A01, ORU^R01 and QRY^A19 (version 2.3)
  to NetGet's endpoint: AA with text, AE with an ERR, AA with QRD/PID response segments; every
  MSA-2 echoes the control id and every ACK is addressed back to the sender.
- `wire_test.rs` — parsing/refusals, Latin-1 fallback, `\F\` escaping and segment-forgery
  refusal, ACK construction, framing refusals (no start block, bad end block, two frames,
  over-long), the NetGet pair (AA/AE/AR and sender-side refusals), a handler-less endpoint
  answering AE, an unparseable message closing the connection, and a mismatched MSA-2 ending
  the sender.

`tests/client/hl7/peer_test.rs` — NetGet's sender against python-hl7's MLLP server (AA, AE+ERR,
AR), with the receiver's own parse of each message asserted.
