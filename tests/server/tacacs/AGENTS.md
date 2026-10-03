# TACACS+ independent server evidence

`install_peers.py ROOT` uses an existing Go>=1.17 and Python interpreter, verifies
bounded archive/wheel SHA256s, extracts ordinary relative files, builds a small
public-API wrapper against unchanged SDK sources, and extracts isolated Python
packages. No global install, source patch, container or new Cargo dependency.
The SDK is a peer implementation, not an official TACACS daemon.

Required pins:

- nwaples/tacplus v0.0.3, revision01141c615540e7ae8bf5ca1b412d0788cb34222b,
  BSD2,14827byte source SHA256
  ced63f2d09fbde9b5fdca3b52978a270731c83cbdc0dc3f35720e76ae4696c80.
  [Upstream](https://github.com/nwaples/tacplus/tree/v0.0.3).
  Public client/server APIs exercise ASCII, PAP, authorization and accounting.
  Mux is disabled; upstream's sequence-wrap guard is commented out, so its finite
  selected exchanges do not establish the native no-wrap boundary.
- ansible tacacs_plus2.6, BSD3,17786byte wheel SHA256
  55aa4e733b0c4366cf5ab2d36deb03729466554319b239b9221b509b256128ff.
  [Upstream](https://github.com/ansible/tacacs_plus/tree/2.6).
  Archived upstream; unchanged public API requires a fresh client per AAA
  operation because PAP leaves minor1 selected. Published wheel/sdist omit the
  license; installer separately verifies the unchanged pinned tag's LICENSE,
  SHA2566b6fbecdde41901e6305b988b09bc0aba3adbb47200c9cfe9938f30ac451cbca.
- six1.17.0, MIT,11050byte wheel SHA256
  4721f391ed90541fddacab5acf947aa0d3dc7d27b2e1e8eda2be8970586c3274.
  Required by the tacacs_plus wheel even though PyPI JSON omits requires_dist.

ROOT retains licenses, unmodified source/wheels, versions and build log.
NETGET_TACACS_GO_CACHE may reuse an existing programme-owned Go compilation
cache. Installer prints NETGET_TACACS_PEER, NETGET_TACACS_PYTHON and PYTHONPATH.
Peer tests fail when required peers are absent, never skip. Native server is
queried by both SDK and Python public clients, with good/bad passwords and
START/STOP/WATCHDOG/UPDATE accounting records. The SDK wrapper's own test policy
is separate from its unmodified codec/session implementation.

`peer_wire.json` contains10 actual packets captured through a transparent proxy
from the unchanged Python2.6 client to unchanged nwaples0.0.3 receiver APIs:
ASCII START/CONTINUE/GETPASS/PASS, PAP START/PASS, authorization REQUEST/REPLY,
accounting REQUEST/REPLY. Fixed test session0x01020304 and secret test-secret;
Python's actual default privilege0 is retained. Wire bytes are not normalized
or rewritten. Each literal plaintext is independently asserted; codec tests
require exact decode and encode equality plus typed-field encoding equality.
Fixture raw hex stays test-internal and never enters product events/actions.
This is cross-implementation golden evidence, not a native self-roundtrip.

Additional preparation calibrated unchanged vitalvas/gotacacs v0.5.0 MIT:
revision70b63f6ddebcb17ee917eb681f8bfa275d056762, source SHA256
1969e31b172c9ed5789a2977cb8e5ba4edf6911b3464a02da44ccb129051a5df.
Its generic PAP Authenticate API incorrectly emits minor0, rejected by nwaples;
no source patch or positive PAP claim is based on that API. Configured mutual
TLS1.3 ASCII/authz/accounting peer calibration is research only and does not
claim native RFC9887 implementation or full compliance.

Run both feature targets at100threads through the programme's shared Cargo
wrapper. Tests cover literal argument order/first separator, caps/enums/flags,
no-wrap, malformed headers/body/continue, default fail-closed policy, handler
failures with a valid SUCCESS action, duplicate/wrong replies, exact-IP secrets,
record-before-ACK access evidence, parser/handler timers, peer EOF and removal
releasing owned sockets/intercepts/listener. Shared privacy regressions cover
constructed deep JSON, malformed model output and script stderr with ordinary
non-sensitive controls. No fuzz or production-capture evidence is claimed.
