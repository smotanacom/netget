# ADC / ADCS hubs — client

Status: Experimental. Cargo feature: `adc`. Both roles are registered through the protocol registries.

Anonymous BASE/TIGR negotiation, CID/PID validation, connection-scoped identity, handler-approved chat, search/results and DCTM/DRCM routing.

No GPA/PAS password login, UDP search or NAT traversal. ADCS uses implicit TLS with certificate validation.

The codec lives in `src/server/adc/codec.rs`; the connecting role uses its Scanner. Each action is declared in the role's `actions.rs`. Shared `p2p_support` owns TCP/TLS tasks, connection admission (256), frame deadlines and bounded handler/notification queues (32). First connection read: 30 seconds; idle: 600 seconds; handshake/command/frame/write: 10 seconds. Handler time is excluded from wire deadlines; owner shutdown cancels it. Malformed/truncated/oversized framing closes the transport. There is no persistent protocol content or account store.

Optional implicit TLS is configured with `use_tls`; listeners require PEM `cert_path`/`key_path`. Clients validate certificates using public roots and optional PEM `ca_path`, and can override the expected DNS `server_name`. Raw TCP wrapping for protocols without a standardized TLS form is a local testing facility.

Tests use the independent ncdc 1.25 and uhub 0.8.0 stack, not the NetGet role pair as interoperability evidence. See `tests/client/adc/AGENTS.md`. The roadmap records completed release gates; metadata remains Experimental.

Specification: https://adc.sourceforge.io/ADC.html
