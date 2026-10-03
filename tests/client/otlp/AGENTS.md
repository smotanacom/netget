# OTLP exporter evidence

Run under the programme's serialized Cargo guard, with `NETGET_OTELCOL_BIN` pointing
at official core `otelcol 0.162.0`, `--no-default-features --features otlp --test client
-- otlp:: --test-threads=100`. The peer is mandatory; absent or wrong version is a failure.
No container, external endpoint or telemetry storage backend is used.
Final local evidence: all nine client checks passed at 100 test threads with no failed or
ignored checks. The combined receiver run also passed all thirty checks.

The fixture starts the Collector with a debug exporter and all three signal pipelines.
Only the selected HTTP or gRPC receiver is enabled, internal metrics are disabled, and
the Collector binds 127.0.0.1:0 itself. The fixture reads the actual endpoint from pinned
Collector startup JSON after its readiness message. There is no reserve/drop port race.
Detailed debug output proves the peer decoded span/log content, resource and item attributes,
integer and floating gauge values, counts and service names. Test-owned child processes use
kill_on_drop and explicit wait; their configuration/log/certificate files live in TempDir.

Collector pin: [official 0.162.0 release](https://github.com/open-telemetry/opentelemetry-collector-releases/releases/tag/v0.162.0).
Darwin arm64 archive SHA256
`53de34c02913f54aa240861e0febf67a6839bc8ecf1bfab83a8ae7d355d30674`;
Linux amd64 archive
`f99929987a915d3c6b2c9b15bc4938c5cea903a37a3f49e478024a0fa0772339`.
TLS fixtures require openssl and generate a localhost end-entity certificate (CA:false,
serverAuth); tests add its trusted PEM explicitly and never disable verification.

`export_test.rs` exercises:

- Official Collector: all 3 signals, HTTP and gRPC, plain and gzip; independently decoded
  names/content/attributes/numeric values and successful acknowledgements.
- TLS: trusted PEM and localhost name succeed; wrong hostname and untrusted certificate
  fail on both transports; client state carries actual socket addresses. A trusted CA file
  exactly 1 MiB succeeds, one byte over is refused, and a directory is refused before reading.
- NetGet pairing: shared connected-handler set_memory, a Python response handler reading
  that memory, accepted exports, partial success, 429 retry delay 7 s, permanent 413 and no
  retry of partial success on either transport.
- Typed validation: item/attribute/pathological text counts, IDs, times, kind, severity,
  nested attributes, signed integer overflow, aggregate 1 MiB and no raw payload action.
- Manual response handlers: 16 occupy the bound, the next export fails busy, disconnect
  remains responsive and all intercepts/command handles disappear.
- Automatic responses requesting an unlimited export chain stop after four follow-ups;
  shared handler memory records the final response and injected commands remain usable.
- Whole export deadline, idle timeout, client removal during a parked RPC, cancellation
  from the peer's side, and stalled TLS handshake deadlines on both transports.
- HTTP response bodies exactly 4 MiB and 4 MiB+1, plain and gzip, malformed protobuf,
  and 512-byte semantic diagnostics. The shared tonic patch suite covers the corresponding
  gRPC receive boundary independently of this runtime.

Access-log helpers reverse the newest-first AppState buffer before indexing semantic events.
Deterministic handlers use a dead model endpoint; these tests require zero model calls.
