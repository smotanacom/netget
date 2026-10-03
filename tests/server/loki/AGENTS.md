# Loki independent peers and checks

Fail when peers are absent; do not add ignored tests, return-early gates or availability
skips. Native checks cover all carriers, Unicode/escaping/string metadata, exact timestamps,
parser and decompression bounds, tenant/token/route errors, explicit260/4xx/5xx responses,
failed actions alongside valid acceptance, shared memory, model suppression, owned-task
cancellation and manual/live injection. Declare Experimental; no fuzz/pcap evidence.

Pinned official Loki3.7.8 (AGPL-3.0) is an unmodified external service oracle, not a runtime
dependency. Native client writes JSON/gzip JSON/protobuf-Snappy, then queries actual stored
lines/timestamps/labels/metadata and checks tenant isolation plus real401/400 errors. Official
Alloy1.20.1 (Apache-2.0) `loki.write` independently encodes protobuf/Snappy from its own API
source, preserving incoming timestamps and metadata. The native collector must observe its
exact typed values; the writer must observe a400 rejection. The source HTTP input is test
stimulus, not an encoder oracle. Processes own temp config/data/log paths, bind only loopback,
disable telemetry/reporting and terminate on completion/drop. Loki's ephemeral ingester fixture
disables WAL; this tests ingestion/readback, without a durability or restart claim. Readiness probes
are bounded; Loki uses a distinct probe stream so an inactive ingester ring cannot pass.

A second independent MIT Python JSON writer, python-logging-loki0.3.1, exercises its public
v1 emitter against the collector (Unicode/labels/timestamp/204 and real400 exception). This
older test-only library does not implement structured metadata; maintained Alloy covers it.
The isolated dependency graph pins requests2.34.2(Apache-2.0),rfc3339 6.2(ISC),
charset-normalizer3.5.2(MIT),idna3.20(BSD-3-Clause),urllib3 2.8.0(MIT),certifi2026.7.22(MPL-2.0).
No global installs, containers or generated protocol implementation dependencies.

```sh
/opt/homebrew/opt/python@3.14/bin/python3.14 tests/server/loki/install_peers.py /tmp/netget-loki-peers
export PYTHONPATH=/tmp/netget-loki-peers/python
export NETGET_LOKI_PYTHON=/opt/homebrew/opt/python@3.14/bin/python3.14
export NETGET_LOKI_PEER=/tmp/netget-loki-peers/loki-v3.7.8
export NETGET_ALLOY_PEER=/tmp/netget-loki-peers/alloy-v1.20.1
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features loki --test server --test client -- loki:: --test-threads=4
```

CI uses its selected Python interpreter instead of the local Homebrew path. Bootstrap takes
one owned root, verifies official archive SHA256 before extraction/execution, extracts only
one executable per ZIP (no extract-all), verifies pinned licenses/version output, installs
Python only into ROOT/python and emits exports/versions.json. Supports Linux amd64/macOS
arm64; unsupported platforms fail explicitly. Official daemon/Alloy tests are required on
Linux/macOS without missing-peer skips; Windows native/Python coverage has no service claim.

Primary sources: [Loki release](https://github.com/grafana/loki/releases/tag/v3.7.8),
[Alloy release](https://github.com/grafana/alloy/releases/tag/v1.20.1),
[Loki license](https://github.com/grafana/loki/blob/v3.7.8/LICENSE),
[Alloy license](https://github.com/grafana/alloy/blob/v1.20.1/LICENSE),
[maintained writer API](https://grafana.com/docs/alloy/latest/reference/components/loki/loki.write/),
[Python0.3.1 package](https://pypi.org/project/python-logging-loki/0.3.1/).
Archive SHA256 values are fixed in install_peers.py from official release asset digests:
Loki Darwinarm64 95d830437482aba989a7d7a65be3c618a67a6e46f6e614544cc4b9ce09f499a1;
Linuxamd64 62aea42c9cba52cd1642b3666ab37019a0ce4c24ab50b07e85dccc8d812f7d61;
Alloy Darwinarm64 9709de08e15ef4307ce52dd5c306edf0d115db02119a712c39c00a04e013e88d;
Linuxamd64 451fe650e8277d22d69cb8db50bba809f581fe78decba7fce4027ef185457be9.

Validation: the guarded Loki suite passed 29 tests (9 client,20 server), zero failed/ignored,
with all pinned peers above. A subsequent focused capacity regression passed: the collector's
503 refusal is parsed by the native client as a typed error with Retry-After5 while256 peers
hold slots, and releasing a malformed peer admits a valid request. Standalone feature
check --tests, correctness/suspicious clippy and final whole-package fmt check pass.
