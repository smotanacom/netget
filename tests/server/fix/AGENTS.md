# FIX tests

Peer: `python3 tests/server/fix/install_peers.py ROOT` builds `qfgo/` (QuickFIX/Go 0.9.12, pinned
by go.sum) and copies FIX44.xml from the verified module; it prints `NETGET_FIX_QFGO` and
`NETGET_FIX_DICTIONARY`. QuickFIX validates every application message against the dictionary
before its application sees it, so a message reaching qfgo's output is a valid FIX 4.4 message.
`tests/helpers/fix.rs` holds the acceptor policy, a raw session for wire tests and the launcher.

- `peer_test.rs` — qfgo as initiator: logon, an ExecutionReport and a BusinessMessageReject,
  heartbeats at HeartBtInt 1, no session Reject from NetGet, logout; a refused SenderCompID.
- `wire_test.rs` — not-a-Logon first, a wrong TargetCompID, the logon timeout, TestRequest,
  a garbled CheckSum ignored, ResendRequest answered with a gap fill and PossDup resends, gap
  detection and a held message resent, an injected News, MsgSeqNum too low; the heartbeat
  timeout, BusinessMessageReject reason 4 without a model and a Logon with no decision; the
  NetGet pair.

`tests/client/fix/peer_test.rs` — NetGet's initiator against qfgo as acceptor.
