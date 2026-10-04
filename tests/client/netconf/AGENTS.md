# NETCONF client tests

`real_server_test.rs` points NetGet's client at the **netconf 2.1.0** Python server
(independent, Apache-2.0, on sshutil 1.5.0 and Paramiko 3.5.1), unchanged, started by
`tests/server/netconf/peer.py server`. The client's own handlers drive get-config →
edit-config → get → get-config → close-session; the edit is read back, and the server's own
JSON log of the RPCs it handled — and on which base version — is asserted, over base:1.0 and
base:1.1. A third test proves a mismatched host-key pin refuses the server. Needs
`NETGET_NETCONF_PYTHON` from `tests/server/netconf/install_peers.py`; fails without it.

The NetGet-pair, lifecycle and injection tests live in `tests/server/netconf/session_test.rs`.
