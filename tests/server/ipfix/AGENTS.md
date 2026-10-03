# IPFIX collector checks

`codec_test.rs` uses a hand-written literal wire oracle in both directions,
typed golden values, all reduced unsigned widths, variable/fixed Unicode and
NUL strings, options scopes, duplicate elements, legal nonzero padding,
unknown fields, truncated frames, transactional rejection, exporter-port/domain
isolation, sequence gaps/late/wrap, refresh/redefinition/ignored UDP withdrawals,
TTL/session expiry and exact message/set/record/field/session/template limits.

`e2e_test.rs` exercises actual sockets: silent/default-no-model collection,
malformed datagram rejection and recovery, parsing independent of a parked
manual handler, 32-message queue capacity, owned socket/intercept cleanup,
actual TTL expiry, typed model opt-in and script/common-memory action failure.
`real_client_test.rs` requires the unmodified public Python exporter to send
IPv4/IPv6, ports/protocol, reduced/full counters, seconds/milliseconds, UTF-8/NUL
strings and an options template; assertions compare native typed values to
literal expectations rather than the native encoder.

Bootstrap the small external test peers into an owned temporary directory:

```sh
python3 tests/server/ipfix/install_peers.py /tmp/netget-flow-peers
export PYTHONPATH=/tmp/netget-flow-peers/python
export NETGET_IPFIX_PYTHON="$(command -v python3)"
export NETGET_IPFIX_COLLECTOR=/tmp/netget-flow-peers/goflow2-v2.2.7
cargo test --locked --no-default-features --features ipfix --test server --test client ipfix::
```

The installer explicitly supports Linux amd64 and macOS arm64, fails on other
platforms, checks bounded official binary downloads against pinned SHA256,
checks `-v`, records versions/license files, and installs no global package.
Python ipfix 0.9.7 source SHA256 is
`31b16fc288819878c2c1845aa1832c8105de47dfbde6af5a5c0500d09b0e940b`;
only ordinary unmodified package `.py`/`.iespec` files are extracted into
`ROOT/python`. It is an LGPL-3.0-or-later external executable test peer, not a
NetGet runtime library. GoFlow2 2.2.7 is BSD-3-Clause; Darwin arm64 binary SHA is
`6bc188842983edbf2df26788180bce3ee801788fec3aebedcff1f5d96eb9fd57`,
Linux amd64 SHA is
`63d3bb6c4e458f56ae3268eac74979da1fee0cea5342362295e7d587784af14b`.

Missing required peers fail, with no ignore or silent skip. Native and Python
tests can compile elsewhere when supplied a working Python peer; no official
Windows/other-platform daemon evidence is claimed. Use the programme's shared
guard wrapper instead of plain Cargo in the expansion worktrees. No pcap/fuzz,
full information-model, reliable-delivery or persistence evidence is claimed.
