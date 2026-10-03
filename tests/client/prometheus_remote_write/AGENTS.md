# Remote Write 1.0 emitter checks

Required `peer_test.rs` drives actual official Prometheus3.15.0 with remote-write
receiver enabled, then reads its real temporary TSDB through PromQL. It asserts
ordered finite samples, NaN/+Inf/-Inf, exact stale-marker disappearance and earlier
history, and a400 duplicate-timestamp/different-value rejection without retry.
This is service evidence, not a custom HTTP receiver or self-roundtrip. The
opposite-role server peer suite tests its real sender. Bootstrap/environment pins
and Apache-2.0 license/owned daemon cleanup are in the server test documentation.

Native `e2e_test.rs` checks shared memory and typed native-pair events, identical
retry bodies and backoff on500/503, every2xx acceptance including a reserved binary
body, terminal400/401/404/default429/307, optional429 retry, busy-write rejection,
command injection/disconnect during a parked handler/backoff, no-wire atomic
validation,32event/action caps, followup depth8,64KiB response and64/32KiB header
bounds, actual10s IO deadline, and removal cancelling owned HTTP and intercepts.
Fixtures are intentionally low-level HTTP peers for edge cases, not independent
protocol implementations. Missing official services/exporter versions hard-fail.
No2.0, metadata/exemplar/histogram, durable buffering, exactly-once delivery,
automatic stale discovery, full-agent/conformance, fuzz or capture claim.
