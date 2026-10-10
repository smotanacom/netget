# PFCP server tests

The peer is **wmnsk/go-pfcp v0.0.24** (go.sum-pinned), in `peer/` (`main.go`). It is one
binary with two modes:

- `smf`, which drives a UPF;
- `upf`, which is one, for the client tests.

`install_peers.py <dir>` builds it and prints `NETGET_PFCP_GO_PEER`. Tests fail rather than
skip without it.

`real_client_test.rs` uses a python policy as the model: it accepts everything and gives
each PDR that asked with CHOOSE the TEID 4096 + pdr_id.

- `go_pfcp_smf_drives_netget_upf`, in order:
  - Establishment before association: cause 72, answered by Rust.
  - Association and heartbeat.
  - Establishment: the response is addressed to the SMF's SEID 0x1111, a UP SEID is
    allocated, and PDR 1 gets TEID 4097.
  - Modification, then the same bytes again: an identical response from the cache.
  - Deletion, then deletion again: cause 65.

  It also checks what the model was shown: go-pfcp's IEs decoded into readable values, and
  the retransmission not reaching the model twice.
- `responses_through_the_pcap_oracle_and_bounds`:
  - An association and an establishment from a raw socket, with tshark's pfcp dissector
    reading every packet.
  - A depth bomb dropped while the server goes on answering.
  - Version 2 answered with Version Not Supported.
- `codec_round_trip`: every readable IE kind and an unknown `ie_200` survive encode → parse.
  Unknown names and causes are refused.

Note: go-pfcp's convenience getters (`FTEID()`, `OuterHeaderCreation()`) do not look inside
PDI or Forwarding Parameters. The peer walks the nested IEs itself.
