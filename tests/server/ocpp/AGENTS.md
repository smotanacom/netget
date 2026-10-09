# OCPP tests

Peer: `python3 tests/server/ocpp/install_peers.py ROOT` (python ocpp 2.1.0, hash-pinned wheel,
with websockets 15.0.1; Python ≥ 3.11) prints `NETGET_OCPP_PYTHON`. `peer.py` drives its
public ChargePoint API in both roles; the library validates every message against the OCPP
JSON schemas, which makes it strict about what NetGet sends.

- `peer_test.rs` — python charge points (1.6 and 2.0.1) against NetGet's CSMS: boot (status,
  interval), heartbeat, status, authorize, StartTransaction/StopTransaction (transaction id 7)
  or TransactionEvent Started/Ended, meter values, a NotSupported CALLERROR surfacing in the
  peer, then a CSMS-initiated remote start sent with `send_to_peer` that the peer accepts.
- `frame_test.rs` — frame parsing/encoding and id-aware refusals, core fields and per-version
  spellings, charge-point ids; a raw WebSocket refused without/with an unoffered subprotocol,
  the occurrence and format errors, a handler-less CSMS answering InternalError; the NetGet
  pair over both versions with a CSMS-initiated Reset and charge-point-side refusals.

`tests/client/ocpp/peer_test.rs` — NetGet's charge point walking both versions' workflow against
python ocpp's central system, which sends Reset after boot.
