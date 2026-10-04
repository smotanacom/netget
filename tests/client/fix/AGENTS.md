# FIX client tests

`peer_test.rs` runs QuickFIX/Go 0.9.12 as acceptor (QFGO accepting CLIENT; independent,
unchanged, validating against FIX44.xml) and drives NetGet's initiator: logon, a NewOrderSingle
answered with an ExecutionReport, an OrderCancelRequest answered with a BusinessMessageReject,
and a logout, checking from qfgo's own output that both messages passed its validation. Needs
`NETGET_FIX_QFGO` and `NETGET_FIX_DICTIONARY` from `tests/server/fix/install_peers.py`.
