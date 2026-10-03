# Prometheus client validation

Run through the serialized programme build wrapper:

```sh
PYTHONPATH=/Users/matus/dev/netget/.protocol-expansion-20261001/queue-peers/python:$PYTHONPATH python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features tcp,prometheus --test client --test server -- prometheus:: --test-threads=100
```

Validated locally at 100 threads: 20 client + 18 existing exporter server tests,
0 failed/ignored. Five shared source targets add 20 passing checks (event wiring,
owned tasks, startup defaults/drift and suite hygiene). Formatting passes. The
server's real idle/parked deadline check takes about 123 seconds; it is executed.

Peers are required, with no skip/ignore gates. Local binaries are
`/opt/homebrew/bin/prometheus` and `/opt/homebrew/bin/promtool`, version 3.15.0,
Go 1.27.1. The independent Python exporter uses `python3` from PATH and official
`prometheus-client==0.22.1` installed into the programme-owned PYTHONPATH above;
the TLS refusal fixture additionally requires `openssl` from PATH. Linux CI may
install the pinned package in its own Python peer directory and set PYTHONPATH.
No global Python installation or Docker resource is required.

The daemon owns its own temporary config/TSDB through RealServer. Its real runtime
gauge/counter/histogram/summary exposition and build-info labels are parsed in text
mode and auto fallback. Prometheus 3.15's own `/metrics` uses `promhttp.Handler()`
with OpenMetrics disabled; strict OpenMetrics correctly emits a refusal, without
accepting its text fallback as the requested format. Primary source:
https://github.com/prometheus/prometheus/blob/v3.15.0/web/web.go.

The official Python client owns exposition negotiation, HTTP serving and rendering
in a separate guarded process. It exercises text and OpenMetrics with counters,
created series, histograms, summaries, NaN, info/stateset and real exemplars, plus
hostile label escaping. The values alone are supplied by the fixture. Source:
https://github.com/prometheus/client_python/tree/v0.22.1,
https://pypi.org/project/prometheus-client/0.22.1/.

The NetGet exporter/client pair covers all existing server types in text and
OpenMetrics, preserved labels and timestamp unit differences. Existing server
tests independently drive promtool and the Prometheus scraper in both formats;
they remain part of the both-role run. Published static/script startup examples
execute against the real daemon; a mocked model chooses a real scrape, stores
shared memory, and the metrics followup sees it. All mock expectations are awaited
and verified.

Parser tests cover exact sample/family/name/label/text/body bounds, duplicate and
interleaved series, metadata/type collisions, malformed escapes/UTF-8/values,
classic millisecond and OpenMetrics second timestamps, special numbers, histogram
components/consistency, UNIT/created/exemplar semantics and required EOF.
Session fixtures prove fragmented responses survive busy injection, request
headers/path defaults, malformed/unsupported/partial responses and recovery,
redirect refusal, Content-Length/chunked body limits, whole scrape deadline with
peer EOF, owned task cancellation/removal with a parked manual event, bounded
event queue/followup chain, fresh injected requests, and rejection of an untrusted
HTTPS exporter certificate. This is Experimental; no fuzz/pcap claim is added.
