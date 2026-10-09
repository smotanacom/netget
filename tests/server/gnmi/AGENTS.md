# gNMI receiver validation

The two `e2e_test.rs` tests require actual independent peers; absence or a wrong
version fails. `NETGET_GNMIC` points to gNMIc 0.49.0, verified by its exact
version line. `NETGET_GNMI_PYTHON` (or `NETGET_GRPCIO_PYTHON`) points to Python
with grpcio 1.75.1, grpcio-tools 1.75.1 and protobuf 6.32.1, pinned by
`tests/helpers/grpcio-peer-requirements.txt`. The helper verifies all versions
and both upstream schema SHA256 values before public grpc_tools generation.
Owned peer children use bounded readiness/output and are killed on drop.

The public SDK client independently encodes Capabilities/Get/Set, ONCE/POLL/
STREAM, updates_only and explicit errors. Tests inspect typed server events
and compare full-width unsigned values, qualified/keyed paths and Poll events.
gNMIc drives verified TLS Capabilities/Get/Set/ONCE, with ProtoJSON assertions
for capability version/encodings, counter value, path qualifiers/keys, Set
operation order and update-before-sync. A second TLS client uses the public
SDK. These fixtures implement no device datastore or YANG execution; the
OpenConfig fake Agent is not used as evidence for RPCs it does not implement.

The helper pins the gNMIc Darwin archive to SHA256
`afd1de25b2d5f524c14f61c2bdcbdc3a515b4c8390b401fb513f2ae549496777`;
the Linux amd64 archive is
`c0b0c59a6956a9f23878063e91900e18093e7306ccc511f0e9047a890b61c7ec`.
The CLI embeds OpenConfig gNMI v0.14.1. TLS uses a temporary real CA and a
separately signed localhost server leaf, never an insecure verifier.

Standalone `gnmi_bounds_test`, `gnmi_semantic_test`, `gnmi_tls_input_test` and
`gnmi_wire_test` exercise structural protobuf, JSON, typed paths/values/model
actions, regular-file/FIFO startup, encoded/expanded 1 MiB and limit plus one,
malformed/opaque/repeated input, unary extra frames, duplicate grpc-timeout,
status-only permit release, 64-RPC admission/reset/stop, partial frames,
response backpressure, 256-TCP admission/recovery and actual 30/120-second
timers. The actual idle watchdog has six-second resolution; logs print close
times. Raw wire probes bypass NetGet's outgoing codec.

Evidence is retained under `.protocol-expansion-20261001/logs/item04-gnmi-*`.
Initial compilation failed E0521/E0282 before checks. The first unprivileged
network run executed two tests that failed socket EPERM. The first allowed
loopback run exposed a real trailers-only END_STREAM wrapper defect and an
invalid CA fixture; both were corrected. A subsequent one-pass/one-fail run
showed gNMIc's explicit disabled QoS zero, which is now accepted. The corrected
server peer run passed both tests at 100 threads. The final whole peer run
`item04-gnmi-peers-body-deadline-final.log` passed 11 client and 2 server checks
at 100 threads, including mandatory gNMIc/public SDK TLS and both roles.
`item04-gnmi-bounds-wire-final.log` passed 6 structural, 7 semantic, 2 TLS-input
and 9 wire checks, plus 5 helper checks and 5 startup/executable-example checks.
The measured first-byte close was 30.003229667 seconds and idle close
120.047133791 seconds. In total these runs contain 37 gNMI checks plus 10
helper/example checks, with no failures or ignores. The earlier wire target
compile error ran no checks; its differing fixture result types were corrected.
No initial failure is erased or reported as a pass.

All-target lint initially found a redundant one-shot select loop in the raw
deadline fixture, then exposed a helper path that assumed helpers lived at the
crate root. The corrected helper uses its sibling module so it also compiles
in the existing nested mock-server harness. Both failed lint logs are retained.

The action declaration audit on the original stable base reported no gNMI
violations, but failed inherited generic-gRPC descriptions/templates (and has
an existing all-feature-only ignored baseline diagnostic). The refreshed
master gate passed after a signed no-ff refresh from published
`246d26ca69b1bc3585f1c5e8ed79b265c5baebcd`; the initial audit failure is retained
separately from all positive protocol evidence.

The refreshed `item04-gnmi-refreshed-peers-neighbors.log` passed complete
client/server targets (32 + 42): 13 gNMI, 38 native gRPC and 23 TCP checks.
`item04-gnmi-refreshed-bounds-shared-corrected.log` passed 96 checks across
17 targets: 24 gNMI, 5 helper and 67 shared checks. Thus the refreshed tree
passed 37 gNMI checks, 61 protocol neighbors and 72 helper/shared checks.
The one ignored check is the existing all-feature-only action baseline
diagnostic, never a gNMI peer skip. Actual first-byte/idle closes were
30.002103750 and 120.051689375 seconds; the local whole-RPC body deadline
was 1.005730833 seconds with reset and recovered-call proof. Refreshed
all-target lint and whole formatting passed. The first shared command named
a nonexistent target and ran zero checks; its argument-error log is retained.

The subsequent TCP 9339 and metadata follow-up passed all 13 gNMI peer checks,
13 declaration/example/port checks and all-target lint. Port-zero fixtures
remain explicit and passed. `well_known_port_declaration_test` contributes
five checks; the other eight shared checks repeat the refreshed example and
action gates. Logs are `item04-gnmi-port-{peers,shared,clippy}-final.log`.

This selected-scope evidence supports Experimental only. There are no ignored
tests, peer absence skips, fuzz execution or pcap conformance claims.
