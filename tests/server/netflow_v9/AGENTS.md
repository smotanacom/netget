# NetFlow v9 collector evidence

Run the feature's server/client targets together with100test threads. Tests
require unmodified softflowd1.1.1 (upstream ENABLE_LEGACY) and actual GoFlow2
2.2.7; missing peers fail, never skip. `install_peers.py ROOT [EXISTING_GOFLOW2]`
supports Linux amd64/macOS arm64 explicitly, requires existing cc/libpcap
headers/library, generates only host config and compiles unmodified source.
It pins the108262-byte source archive SHA256
`a6882e59931e5880901f8ee28d78b082cb3000ad8d28af35c13f2b528edbb2c9`.
GoFlow2's platform binary is hash-checked even when reused read-only. License,
version, configuration/build logs remain in owned ROOT; no global install,
containers, autotools or source patch. On Linux CI provide libpcap-dev and cc.
The installer fails clearly if tools, platform, downloads or digests disagree.

Set NETGET_NETFLOW_V9_EXPORTER and NETGET_NETFLOW_V9_COLLECTOR to its printed
paths. The helper uses owned temporary pcap/output/socket paths, foreground
softflowd `-a -d -r ... -v9 -Pudp -c none`, kill-on-drop and bounded deadlines.
The212-byte actual exporter wire is compared with `softflowd_ipv4.hex` except
its runtime UNIX header seconds, which are independently checked. Native
typed values and the actual GoFlow2 service must agree with literal expected
addresses, ports, byte/packet counts, first/last switched and scope identities.
GoFlow2 JSON data bytes are decoded only within the external peer test; raw
bytes never enter model-facing native event/action fields. The upstream modern
sender Count defect is documented; enabling its unmodified legacy sender is
a build configuration choice, not a patched wire or self-roundtrip claim.

`v9_ipv4.hex` is an independent RFC-layout golden asserted in both directions.
Codec checks cover normal/options headers, Count, fixed scalar widths, unsigned
counter widths, IPv6, padding, unknown full16bit fields, no enterprise/variable
interpretation, malformed transactional state, clock regression, uptime
ambiguity, expiry, Source-ID/IP isolation, source-port continuity, sequence
wrap/late/gap including unknown templates and all cache/message bounds.
Lifecycle checks use real UDP sockets and common dispatch: no default model,
no replies, actual terminal decision tags, script memory, failed action/backend
handling, bounded queue parsing while a handler parks, expiry/redefinition and
removal releasing owned socket/intercepts. Explicit model opt-in uses the
shared mock harness and expects one creation plus one event call. Deterministic
handlers make no model calls. No fuzz or production capture claim is made.
