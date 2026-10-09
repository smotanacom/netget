# SMPP client tests

- `session_test.rs`: against NetGet's own SMSC — a refused bind failing creation, a submit
  with its result, receipt and reply, a rejection reported with its status, a long UCS-2
  text, enquire_link, a bad address refused locally.
- `real_server_test.rs`: Melrose Labs' SMSC simulator (the C++ file gosmpp ships, compiled
  unchanged): bind, a submit with a 64-character id, the immediate mobile-originated echo,
  the receipt 3–12 s later naming that id, enquire_link, and a refused bind. It listens on
  the fixed port 2775.

Two SMSC candidates were rejected: fiorix/go-smpp's smpptest writes deliver_sm with a stray
byte that gosmpp and smpplib both refuse to parse.

`install_peers.py ROOT` installs smpplib (hash-pinned), builds `peer/` (gosmpp, go.sum-pinned)
and compiles the simulator from the go.sum-verified gosmpp module with g++, printing
NETGET_SMPP_PYTHON, NETGET_SMPP_GO_PEER and NETGET_SMPP_SMSC_SIM.
