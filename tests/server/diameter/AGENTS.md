# Required independent Diameter peers

Tests fail if either pinned peer is unavailable. No skip-if-missing path.
`install_peers.py ROOT` requires existing Python>=3.11 and Go>=1.26; it downloads
small pinned archives into owned storage, checks SHA256, bounded safe ZIP paths
and licenses, extracts unchanged upstream sources, and builds only a public-API
adapter. No source patch, monkeypatch, container, global package/tool install,
external Diameter service, or new native NetGet Cargo dependency.

Run with the bootstrap output:

```sh
python3 tests/server/diameter/install_peers.py /tmp/netget-diameter-peers
export NETGET_DIAMETER_PEER=/tmp/netget-diameter-peers/go-diameter-peer
export NETGET_DIAMETER_PYTHON=python3
export PYTHONPATH=/tmp/netget-diameter-peers/python
cargo test --locked --no-default-features --features tcp,diameter --test server --test client diameter:: -- --test-threads=100
```

The programme workspace uses its mandatory serialized `run_cargo.py` disk guard
instead of direct Cargo. `NETGET_DIAMETER_GO_CACHE` may borrow an existing owned
Go compilation cache. The other Go module/download paths stay inside ROOT.

* [python-diameter0.9.0](https://pypi.org/project/python-diameter/0.9.0/),MIT,
  Python>=3.11,no declared runtime dependencies. Wheel288278bytes SHA256
  `b5ef067db631181a06578d8b1f33e8536425b318d8612aa48479ee3f4abf934d`.
  [Upstream Node/application API](https://python-diameter.org/docs/0.9.x/guide/node/)
  provides both real TCP peer roles and owns CER/CEA,watchdog and disconnect.
* [fiorix/go-diameter/v4 v4.5.0](https://github.com/fiorix/go-diameter/releases/tag/v4.5.0),
  BSD3,Go>=1.26. Official Go module ZIP515118bytes SHA256
  `e5947ecb7a80c9ba0c972c11f2c24516c4b67797fbef912307222ccc711e84b3`.
  The unchanged public state machine performs CER/CEA and DWR/DWA; the public
  NASREQ dictionary/message APIs handle typed AAR/AAA. The adapter's typed DPR/DPA
  policy uses SDK Message/AVP/Answer/WriteTo APIs; no adapter wire codec.
  Its only compiled external dependency is Apache2 `ishidawataru/sctp`
  `v0.0.0-20251114114122-19ddcbc6aae2`,28KiB module ZIP, pinned official Go h1
  sums in the bootstrap. TCP tests do not require SCTP kernel support. Declared
  gRPC example dependencies are not downloaded/built. No generated SDK source
  changes, no native NetGet runtime link to either SDK.

Unchanged Python0.9.0 AaAnswer omits Auth-Request-Type and Auth-Session-State
field definitions, and names Service-Type `service_stype`. `peer.py` therefore
uses upstream public `append_avp(Avp.new(...))` for required274, selected277=1,
and6=1. This is typed upstream encoding, not patched source or normalized wire.
The documented semicolon port example differs from its parser: use standard
`aaa://host:port;transport=tcp`. Node's tcp_port0 disables listening, so the
owned fixture first selects an explicit ephemeral port. A failed nonblocking
connection cleanup path produced SO_LINGER EINVAL on macOS during initial
calibration; normal healthy Node.stop was separately verified to exit0. No
general macOS teardown limitation or hidden forced lifecycle success is claimed.

Before native source work, both upstream Node roles completed accepted2001 /
rejected4001 AAA, CER/CEA, both-direction DWR/DWA and DPR/DPA with normal stop.
`peer_wire.json` contains all12 real captured frames without normalization.
The fixture's literal header/AVP assertions checked actual fields and padding;
unchanged Go ReadMessage also decoded all12 with no DecodeErr. Cross-library
Python-client→Go-server and Go-client→Python-server each accepted/rejected PAP
and exited normally. These calibrations establish the peers' selected behavior;
the native tests separately establish NetGet interoperability. Keep any failed
calibration logs in programme-owned evidence storage; do not convert them to
successful native or platform claims.

Native tests cover both independent client/receiver roles, all3selected request
types, actual password readback in typed server events, credential-free client
results, default denial, backend errors, literal frames, malformed/stateful /
uncorrelated successes, unsupported mandatory AVPs, constructed10kdepth input,
packet/handler/request deadlines, injection while a handler is parked, native
peer-control responsiveness, EOF/removal and task cancellation. Child peers use
kill-on-drop plus the shared parent-death tie; normal stop must return success,
and explicit timeout cleanup kills/reaps only the owned child. No fuzz or pcap
production-evidence claim. No tests under src.
