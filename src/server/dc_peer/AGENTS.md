# NMDC file peers — server

Status: Experimental. Cargo feature: `dc_peer`. Both roles are registered through the protocol registries.

Uploader/listener and downloader/connector, lock/key handshake, ADCGET/ADCSND binary file and compressed XML file-list downloads, bounded ranges and optional full-payload TTH verification.

1 MiB per transfer; identifiers cannot contain whitespace. No filesystem access, transfer scheduler, TTH leaf requests or automatic hub discovery. NMDC defaults to plaintext.

The codec lives in `src/server/dc_peer/codec.rs`; the connecting role uses its Scanner. Each action is declared in the role's `actions.rs`. Shared `p2p_support` owns TCP/TLS tasks, connection admission (256), frame deadlines and bounded handler/notification queues (32). First connection read: 30 seconds; idle: 600 seconds; handshake/command/frame/write: 10 seconds. Handler time is excluded from wire deadlines; owner shutdown cancels it. Malformed/truncated/oversized framing closes the transport. There is no persistent protocol content or account store.

Optional implicit TLS is configured with `use_tls`; listeners require PEM `cert_path`/`key_path`. Clients validate certificates using public roots and optional PEM `ca_path`, and can override the expected DNS `server_name`. Raw TCP wrapping for protocols without a standardized TLS form is a local testing facility.

Tests use the independent ncdc 1.25 stack, not the NetGet role pair as interoperability evidence. See `tests/server/dc_peer/AGENTS.md`. The roadmap records completed release gates; metadata remains Experimental.

Specification: https://nmdc.sourceforge.io/NMDC.html
