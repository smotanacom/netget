# A2A client tests

`peer_test.rs` runs a2a-sdk 1.2.1's agent (independent, unchanged) with an echo executor that
creates a task before any status update, and drives NetGet's client through card resolution, a
direct message, a streamed task, GetTask, a working task, CancelTask and a missing task's
-32001. Needs `NETGET_A2A_PYTHON` from `tests/server/a2a/install_peers.py`; fails without it.
