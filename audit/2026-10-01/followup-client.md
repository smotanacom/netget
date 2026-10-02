# Client follow-up implementation and validation map — 2026-10-01

This report follows `client-review.md`. The first report preserves the original inventory of 777 client, easy, pipe, and client-test files and the original findings. This report maps the eleven remaining findings to the subsequent implementation, explains compatibility changes, and identifies what CPU-only checks can establish. It does not replace the original inventory or claim that every protocol received the same depth of review.

All code and tests described here were developed without GPUs, model inference, model downloads, remote protocol services, or device access. The root agent coordinates Cargo builds and test execution; its final validation ledger is authoritative. This reviewer did not start competing Cargo builds. No commits or pushes were made.

## Original finding disposition

| Original remaining finding | Implementation | Evidence and actual boundary |
|---|---|---|
| 1. POP3 reply type guessed from `+OK` wording | Added a command writer that atomically serializes writes with a bounded FIFO of response expectations. Both event-driven and injected commands share it. | Regression checks USER/PASS, LIST/UIDL with and without arguments, RETR/TOP/CAPA, negative replies, unsolicited replies, CRLF injection, and cancellation during a partial write. No real mail server used. |
| 2. VNC type-2 authentication echoes the challenge | Implemented the RFB password-derived DES response, including eight-byte truncation/zero padding and per-byte bit reversal. Password authentication is preferred when a password is supplied and the server offers it. | Independent OpenSSL fixture, truncation, and zero-padding assertions. No remote VNC server or screen capture used. |
| 3. Inconsistent native HTTP response bounds | Both `FetchClient` backends enforce the buffered body policy. Shared bounded reqwest byte/text/JSON helpers also cover direct readers. Body failures are propagated instead of converted into empty successful responses. | Real loopback chunked HTTP response, over-limit buffered error, streaming download preservation, JSON parsing, and Windows-1252 decoding. Browser transport retains its existing whole-body bound. |
| 4. SMB unbounded reads and synchronous work under an async lock | Added an 8 MiB buffered read/write policy and moved native initialization, operations, and destruction onto blocking workers. A lifetime lease protects pavao's process-global native context. | Pure CPU bounded-reader regression; host has a discoverable libsmbclient package. Real credentials/share operations were not exercised. Native blocking calls cannot be forcibly cancelled safely. |
| 5. WebRTC callback ownership, dropped queues, and abort cleanup | Replaced callback self-ownership with weak references, added bounded FIFO draining, propagated a shutdown token into event calls, and introduced an owning peer-close guard. | Regression creates an actual library peer with empty RTC configuration, aborts its owner, and waits for the peer's Closed state. No STUN/TURN service, signaling service, or two-peer data exchange. |
| 6. SSH-agent injected Custom actions do not send packets | Event-driven and injected commands use the same checked SSH packet encoder. An independent registered command task is available while event handling waits for a model/manual action. | In-memory duplex regression observes the exact framed request-identities bytes and the real sent-byte outcome; overflow flags fail. No operator agent or key access. |
| 7. HTTP URL malformed port/userinfo handling | Explicitly reject invalid or empty ports, empty hosts, userinfo, and unescaped ASCII control/whitespace in the shared HTTP transport parser. | Invalid authority cases and valid IPv6 authority/path/query are tested. Native reqwest URLs retain reqwest's semantics. |
| 8. Incomplete-frame timeouts and aggregate reads | Added absolute partial-line/body/frame deadlines for the reviewed text, SSH-agent, VNC, BitTorrent peer, and CONNECT readers, plus relevant greeting/handshake deadlines and native buffered HTTP body deadlines. | Short CPU-only timeout regressions check partial text, explicitly armed frame deadlines, partial torrent headers, and SSH deadlines across cancellation and between polls. These are framing deadlines, not a global idle-disconnect policy. |
| 9. Unchecked binary narrowing | Added shared checked number/byte conversion and applied checked conversions to concrete action/startup fields and outgoing wire lengths listed below. Values that cannot be represented now fail instead of wrapping or disappearing. | Generic boundary regression plus VNC action and SSH packet regressions; feature checks cover other changed gates. Protocol-specific bit packing and casts already protected by bounds were retained. |
| 10. Stale documentation | Updated the affected protocol guides for implemented authentication, correlation, cleanup, bounds, deadlines, native ownership, and truthful command outcomes. | Documentation changes listed below. Historical protocol maturity is not upgraded merely because a module compiles. |
| 11. Hardware/unavailable environment validation | Kept changes testable through pure helpers and local streams and requested native feature checks independently of device access. | Device, privileged networking, browser execution, external interoperability, and runtime-shutdown behavior remain actual environment boundaries, not claims of verified support. |

The optional easy-client execution path was implemented by the runtime/server reviewer in `src/cli/easy_startup.rs`. Its contract accepts existing management forms: `open_server` uses `protocol`/`port`; `open_client` uses `protocol`/`remote_addr`. Startup parameters and event-handler configuration pass through those forms. The existing easy HTTP generator already emits compatible server actions. No additional easy protocol type or invented client metadata schema was necessary in `src/easy`.

## Buffered HTTP response policy

`src/client/http_fetch/mod.rs` now carries the selected body bound through the native request and response wrappers as well as the transport backend. The default remains 8 MiB, and `FetchClient::with_max_body` applies to buffered `bytes()`, `text()`, and `json()` on either backend.

The native helpers are public so callers that own a reqwest response can use the same policy:

```rust
read_response_bytes(response: reqwest::Response, limit: usize) -> Result<bytes::Bytes>
read_response_text(response: reqwest::Response, limit: usize) -> Result<String>
read_response_json<T: DeserializeOwned>(response: reqwest::Response, limit: usize) -> Result<T>
```

These helpers check a declared body length, incrementally account for chunks before appending, and impose a 30-second deadline over the whole buffered read. The text helper preserves reqwest charset/BOM handling by retaining response headers when decoding the already bounded bytes. JSON decoding receives only the bounded complete body.

Native `chunk()` remains streaming and does not acquire a new aggregate download limit. This preserves intentional large-download consumers and their own stream/file policies. The browser/in-memory transport continues to collect its bounded body before exposing chunks. This difference is documented in the shared transport module.

The shared wrapper covers HTTP-family clients that already use it, including package-metadata and OAuth/OIDC paths. Direct MCP reqwest reads now call the shared helper. Bitcoin, HTTP/2, JSON-RPC, WebDAV, and OAuth token parsing now return body/decode failures instead of silently substituting an empty body or null response. Root adopted the helpers in its separately owned LLM tools, model-list, and error-body paths.

Transport URL validation remains scoped to `parse_http_url`. It rejects userinfo rather than silently treating credentials as part of an authority and tells callers to use an Authorization header. Explicit out-of-range, nonnumeric, and empty ports fail instead of defaulting to port 80. Valid IPv6 authority parsing and origin-form path/query are retained. Extension methods retain their exact casing; the seven historically supported standard verbs keep the earlier case-insensitive convenience.

## POP3 command and response ordering

`CommandWriter<W>` owns the writer and its expected response FIFO. A shared mutex makes adding a reply expectation and writing/flushing the matching command one serialized operation across both producers. The queue is capped at 1,024 outstanding commands.

The expectation follows command semantics: RETR, TOP, and CAPA are multiline; LIST and UIDL are multiline only without an argument; other commands are single-line. A negative status consumes the expectation without reading a dot body. The server's prose no longer controls framing. Responses without an expected command fail instead of guessing.

The writer rejects embedded CR/LF, blank commands, and excess outstanding work. It marks itself failed before a write starts and clears that marker only after the write and flush complete. Cancellation or failure can therefore never leave a reusable stream whose queued expectations no longer match what reached the server. Command writes have a 30-second deadline. The existing shared dot reader retains whitespace, removes dot stuffing, bounds retained text, and refuses EOF before the exact terminator.

The reply loop propagates errors through the existing status/handle cleanup path. It does not introduce POP3 pipelining negotiation or new mail semantics; it makes concurrent command producers and received replies agree about the commands that were actually issued.

## VNC authentication and framing

The new `vnc_auth_response` implements the existing RFB password authentication algorithm using the cached `des` crate enabled by the `vnc` feature. It zero-pads or truncates the password to eight bytes, reverses each key byte's bit order, and encrypts each eight-byte half of the server's challenge with DES-ECB. It does not log credentials or retain a fake challenge-echo fallback.

Algorithm behavior was checked against the primary LibVNC implementation and a locally generated OpenSSL DES fixture:

- [LibVNC password/challenge handling](https://raw.githubusercontent.com/LibVNC/libvncserver/master/src/common/vncauth.c)
- [LibVNC OpenSSL DES implementation](https://raw.githubusercontent.com/LibVNC/libvncserver/master/src/common/crypto_openssl.c)

No implementation source was copied. For the ASCII password `password` and challenge bytes 0 through 15, the independently generated expected response is `b866924125c8eebb9debc1db61c538e2`.

The entire handshake has a 30-second deadline. After the initial message-type byte, a binary-frame deadline covers the rest of each message; it is disarmed between frames and during event handling. Existing first-wave text and raw-pixel allocation limits remain. Outgoing clipboard data is capped at 1 MiB, and its wire length is checked. Coordinates and button masks are validated before action results are emitted, preventing a downstream Custom action from receiving already-truncated values.

This is compatibility with the protocol's existing authentication mechanism. It does not add a new VNC security type or claim stronger security for DES-based RFB authentication.

## SSH-agent dispatch, framing, and ownership

`encode_custom_action` is now the single packet construction path used for event results and injected commands. The encoder checks u32 flags and length-prefixed byte strings, refuses oversized messages before sending, and reports actual framed bytes sent. The previous generic Custom-action success without a packet is removed.

The command channel is registered before the connected event. A separately registered command task can send commands even while the event flow waits for a manual action. Event handling snapshots client memory before any model/manual wait. The old unused accumulation state and frame-dropping queue were removed; the serial framed reader and socket backpressure preserve pending frames.

`AgentResponseReader` wraps a length-delimited reader with a progress tracker and an absolute timer. The progress tracker covers the case where the codec consumes all four length bytes internally and its visible read buffer becomes empty while the body is missing. The timer persists when a caller cancels `next()` and resumes it later. An expired timer is checked before a newly completed frame can be accepted, so pausing polls cannot renew or bypass the deadline. Responses remain capped at 1 MiB. Writes have a 30-second deadline.

The default socket remains NetGet's own fabricated-agent socket. This work did not connect to `$SSH_AUTH_SOCK`, enumerate identities, request signatures from an operator agent, or change that safety-sensitive default.

## SMB native ownership and limits

The pavao 0.2.13 source was inspected locally. Its client owns a process-global libsmbclient context, and dropping a client frees that context. Allowing a second independent owner could invalidate the first. NetGet therefore holds a single native-session semaphore lease for the entire native client's lifetime and returns a clear error for a second active SMB session. This is a material compatibility restriction that prevents invalid native ownership with the currently pinned library.

`NativeSmb` retains both the native client and the lease. Its destructor transfers native teardown to a blocking worker and releases the lease only after teardown. Initialization, directory operations, file operations, and teardown run outside Tokio's async workers. Native file handles are created, used, and dropped inside the blocking closure; none crosses a model event await.

`read_file_bounded` reads at most 8 MiB plus one detection byte, returns an error on overflow, and never publishes partial file contents as success. Buffered writes enforce the same cap. Operation errors still generate their error event and are also returned as errors to command callers rather than reported as successful execution. The native client receives a 30-second operation timeout.

Boundaries: the pinned native API returns directory entries as an allocated vector, so this change does not promise a new bound on directory enumeration. A running C-library operation cannot safely be force-aborted; cancellation can leave the blocking operation finishing under the native timeout. No real share, network filesystem, credentials, ACL behavior, or SMB dialect interoperability was exercised. `pkg-config --modversion smbclient` returned `0.8.1` on this host, permitting a native feature check rather than assuming the library is absent.

## WebRTC lifecycle and queues

`PeerConnectionGuard` retains the peer and a cancellation token. Dropping its last owner cancels event work and schedules `peer.close()` on the live Tokio runtime. The command task owns a guard for the client lifetime; setup errors explicitly close and unregister. This gives AppState task abortion a cleanup path instead of relying on a detached task or raw pointer.

Data-channel callbacks upgrade weak references when needed, avoiding the client/channel/callback ownership cycle. Event calls select against the cancellation token so shutdown does not wait indefinitely for a manual/model decision. The active message processor drains its FIFO before returning to idle. The FIFO is bounded by 128 messages and 8 MiB of retained message content; individual incoming messages are capped at 1 MiB. Overflow is reported and closes the channel rather than silently losing queued data. Binary classification uses the library's actual string flag.

The lifecycle test uses a real `RTCPeerConnection` with empty configuration and asserts that aborting the owning task reaches `Closed`. It does not negotiate an ICE connection, use media, or contact STUN/TURN servers. Async close can only run while a runtime exists; abrupt process termination or a fully stopped runtime is outside that guarantee. The existing injected `send_offer` behavior remains explicitly rejected, and stale documentation claiming it was executed has been corrected; adding a new renegotiation command was not part of the identified cleanup defect.

## Framing policies and checked wire values

| Path | Bound/deadline now enforced |
|---|---|
| Shared FTP/POP3/NNTP/CONNECT response line | 64 KiB; 30 seconds after the first byte of a line |
| POP3/NNTP dot response | 8 MiB retained text; one 30-second deadline for the whole body |
| POP3/NNTP greeting | Must begin and complete within 30 seconds |
| CONNECT response head | 64 KiB aggregate; one 30-second deadline over status and headers |
| BitTorrent peer inbound frame | 8 MiB; 30 seconds after first length-prefix byte |
| BitTorrent peer handshake read | 68-byte handshake must complete within 30 seconds |
| SSH-agent inbound frame | 1 MiB; 30-second partial-frame deadline survives cancelled polling |
| VNC handshake and each started server message | 30-second absolute deadline; existing 1 MiB text and 256 MiB raw-rectangle caps |
| Native buffered HTTP body | Selected cap, 8 MiB by default; 30-second whole-read deadline |
| SMB buffered file | 8 MiB; native operation timeout configured to 30 seconds |

Idle connections remain idle where the protocol has no solicited handshake in progress. These changes do not claim to impose a universal timeout on every command, connection attempt, event callback, frame type, or protocol in the repository. FTP's initial idle greeting wait, for example, retains the existing behavior; a started FTP line receives the shared partial-line deadline.

`src/client/wire_values.rs` supplies checked unsigned-field conversion and complete validation of byte arrays. Missing or null optional numeric fields retain their documented default; negative numbers, fractional numbers, wrong types, and values outside the target type fail. Byte arrays must contain only integer values from 0 through 255. Previously filtered invalid entries or narrowing casts could silently construct different packets.

Concrete applications in this follow-up:

| Client | Changed field or length handling |
|---|---|
| MQTT | QoS must fit u8 and be 0, 1, or 2 before creating the action result |
| SSDP | MX is clamped to the existing protocol range before converting to u32 |
| WireGuard | Persistent-keepalive startup value must fit u16 |
| HTTP/3 | Optional action priority must fit u8 |
| ICMP | Identifier/sequence must fit u16; TTL must fit u8 |
| NFS | Read count and mode must fit u32; outgoing write length is checked |
| OpenID Connect | Callback port must fit u16 |
| TURN | Lifetime must fit u32; attribute/message lengths must fit u16; data byte arrays validate every entry |
| UDP and IGMP | Custom byte arrays reject malformed or out-of-range entries |
| gRPC and DNS-over-TLS | Length-prefix conversion checked before writing the wire frame |
| Direct Connect | Maximum reconnect count must fit u32 |
| SNMP | Negative startup timeouts/retries rejected; GETBULK counts checked as u32 |
| Bluetooth and IPP | Characteristic/document byte arrays validate every entry |
| USB | Output byte arrays validate; control-transfer sizes fit u16; rejected validation has an explicit rejected outcome |
| BitTorrent tracker and Ident | Custom port values checked as u16 |
| BitTorrent peer | Message type checked as u8 and outgoing payload/frame length bounded |
| VNC and SSH-agent | Action fields and wire lengths checked as described above |

## Documentation updated

- `src/client/http/CLAUDE.md`: shared buffered body bounds and preserved streaming semantics.
- `src/client/pop3/CLAUDE.md`: command-based response correlation, poisoning after interrupted writes, and deadlines.
- `src/client/vnc/CLAUDE.md`: real DES authentication, framing/pixel policies, checked actions, and handshake timeout.
- `src/client/ssh_agent/CLAUDE.md`: working injected command dispatch, serialized framing, and timeout/cancellation behavior.
- `src/client/webrtc/CLAUDE.md`: owning cleanup guard, weak callbacks, bounded queue draining, and truthful rejected command behavior.
- `src/client/smb/CLAUDE.md`: pinned pavao version, off-thread operations, one-context limitation, file bounds, and cancellation limits.
- `src/client/ftp/CLAUDE.md`, `nntp/CLAUDE.md`, `http_proxy/CLAUDE.md`: actual text/aggregate framing and timeout policy.
- `src/client/torrent_peer/CLAUDE.md`: wire-frame bounds and partial-frame/handshake behavior.

Older descriptions of protocol maturity elsewhere remain historical evidence rather than a promise of testing by this audit. Feature compilation and helper tests alone cannot certify hardware, authentication against external deployments, or browser transport interoperability.

## CPU regression target

`tests/client_followup_test.rs` contains 18 tests when its relevant features are enabled:

1. `wire_values_reject_overflow_and_invalid_array_elements`
2. `http_target_refuses_invalid_explicit_ports_and_userinfo`
3. `bounded_native_response_helpers_keep_charset_and_json`
4. `native_buffered_cap_counts_chunked_body_but_download_chunks_still_stream`
5. `incomplete_text_lines_have_a_deadline_after_the_first_byte`
6. `binary_frame_deadline_excludes_idle_and_completed_event_handling`
7. `pop3_framing_tracks_each_written_command_not_status_wording`
8. `pop3_interrupted_write_poisoning_prevents_wrong_response_correlation`
9. `vnc_des_matches_independent_openssl_fixture_and_password_truncation`
10. `vnc_actions_reject_values_before_custom_results_can_truncate_them`
11. `injected_agent_request_writes_a_framed_packet_and_reports_actual_bytes`
12. `agent_partial_body_deadline_survives_cancelled_next_calls`
13. `agent_expired_deadline_rejects_a_body_completed_between_polls`
14. `peer_partial_headers_have_an_absolute_deadline`
15. `smb_buffered_file_read_refuses_oversize_without_returning_partial_success`
16. `aborted_client_owner_closes_the_webrtc_peer`
17. `agent_incomplete_writes_close_and_poison_the_transport`
18. `agent_write_deadline_covers_lock_wait_without_poisoning_an_untouched_transport`

The first-wave `tests/client_review_regression_test.rs` target remains relevant: it checks the earlier allocation, line/dot framing, HTTP method/driver cleanup, easy Markdown, and pipe fixes. These targets use bounded in-memory data and local loopback sockets. Constructed AppState model URLs use an unreachable local port and are never invoked by these tests.

Suggested focused test invocation, to be run only by the root coordinator through the repository's normal CPU-safe Cargo environment:

```sh
cargo test --offline --no-default-features --features 'http,ftp,nntp,pop3,http_proxy,vnc,torrent-peer,ssh-agent,webrtc,smb-client' --test client_followup_test --test client_review_regression_test
```

Changed-gate check union supplied to the root coordinator:

```text
http,http2,http3,http_proxy,bitcoin,jsonrpc,mcp,webdav,oauth2,openidconnect,pop3,nntp,vnc,torrent-peer,ssh-agent,webrtc,mqtt,ssdp,wireguard,icmp,nfs,turn,udp,grpc,dot,dc,igmp,snmp,bluetooth,ipp,usb,torrent-tracker,ident,smb-client
```

This reviewer ran targeted rustfmt on changed files and `git diff --check` on owned source/test paths, which passed. At this report's creation, root's broad native check and final test runs were in progress. An added test is not recorded here as passing merely because its source exists. Exact feature exclusions, platform build failures, final counts, and command logs belong to the root's final validation ledger.

## Integration cross-review

At the parent's request, this reviewer also read the latest root-owned circuit-breaker lease/generation logic, resizable FIFO rate-limiter gate, session restore, and native file I/O changes without editing those files. No concrete FIFO resize/cancellation or breaker generation regression was found in that read-only review. Three targeted findings were sent to the root for ownership-preserving follow-up:

- Validate that a saved resource action is an object before mutating `action["protocol"]`; scalar/array JSON can otherwise panic in serde_json's string index mutation.
- Validate that an easy wrapper's referenced resource protocol matches its declared underlying protocol, in addition to validating registry metadata and resource existence.
- Install atomic-file cleanup ownership only after `create_new` succeeds, so a failed open cannot remove a file the writer never created.

The root's final diff and validation record determine those findings' ultimate disposition. No shared Cargo, LLM, utility, session, or runtime ownership files were edited by this reviewer during that integration review.

## Windows validation handoff

The root requested an independent Windows compile check after the client work stabilized, using the installed `x86_64-pc-windows-gnu` target, CPU-only `tcp,http` features, two build jobs, debug info disabled, offline resolution, and the isolated target directory `/private/tmp/netget-audit-windows-20261001`.

The initial command selected `/opt/homebrew/bin/rustc` through PATH. That Homebrew sysroot did not include the Windows standard library, although rustup listed the target under its separate stable toolchain. It failed with E0463, `can't find crate for core`. The exact log is `tmp/audit-2026-10-01/windows-check.log`.

The check was rerun using the already installed rustup Cargo and rustc explicitly:

```sh
env CARGO_TARGET_DIR=/private/tmp/netget-audit-windows-20261001 \
  RUSTC=/Users/matus/.rustup/toolchains/stable-aarch64-apple-darwin/bin/rustc \
  RUSTC_WRAPPER= CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 \
  /Users/matus/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo \
  check -p netget --lib --no-default-features --features tcp,http \
  --target x86_64-pc-windows-gnu
```

The second attempt compiled Windows Rust dependencies, then exited 101 in the `ring 0.17.14` native build script because `x86_64-w64-mingw32-gcc` is absent. The exact log is `tmp/audit-2026-10-01/windows-rustup-check.log`. No compiler or target was installed, no speculative cross-compilation workaround was attempted, and the main target directory was not used. **NetGet's Windows ProcessGroup and atomic configuration paths were not reached by this local check and are not recorded as compiled or executed.**

Per the root's fallback instruction, a distinct `windows-resource-checks` job was appended to `.github/workflows/ci.yml`. It runs on `windows-2022`, uses stable Rust and Node 22, and disables default features:

- Rust targets: `core_followup_test`, `llm_rate_limiter_test`, and `llm_config_test`, with `tcp,http` features. This builds the Windows library path and executes portable configuration/concurrency regressions; it does not execute Unix descendant-process tests.
- Node targets: `tests/npm_launcher_test.cjs` and `tests/npm_download_test.cjs`, using harmless local fixtures. The existing Unix shell-cache and Unix signal cases explicitly skip on Windows; the remaining launcher/archive behavior runs on Windows.

This job is added coverage, not a claimed CI result: it was not dispatched or executed from this session. No Windows source change was justified by the dependency-toolchain blocker.

### Isolated Windows ProcessGroup compile evidence

After the full-package blocker, the root authorized a dependency-minimal fixture. This reviewer coordinated it with the surfaces reviewer, who created `/private/tmp/netget-windows-process-fixture-20261001`, imported the exact repository `src/scripting/process_io.rs` source, and checked it for `x86_64-pc-windows-gnu` using the installed rustup compiler. Direct versions were pinned to Tokio 1.48.0, UUID 1.18.1, and windows-sys 0.61.2. The fixture avoids ring and does not modify the main workspace or its target.

The fixture **passed** in 6.89 seconds. This verifies the Windows Job Object API signatures and the exact module's Rust type checking. It emitted dead-code warnings expected from an isolated fixture and an unused-mut warning for a builder whose mutation is Unix-only. No source change was necessary. Log: `tmp/audit-2026-10-01/followup/windows-process-fixture.log`. This is cross-compilation evidence, not execution on Windows, and it does not compile the rest of NetGet or its Windows atomic-file paths.

### Restore startup ownership review

A later read-only review confirmed a gap below `RestoreOwnership`: forms return a resource ID only after startup finishes, while the startup functions register the resource before awaiting memory updates, task registration, protocol startup, and final status updates. Cancellation during those awaits can leave the new resource absent from the outer restore guard. Client startup also retained its newly registered error row on an ordinary connection error, so failed restore could leak that row and its scoped tasks without cancellation. The server error branch removes its row, but cancellation can bypass or interrupt that branch.

The recommendation sent to both the root and runtime/server reviewer was a startup ownership guard installed immediately after `add_server`/`add_client` returns, armed until the final successful return, with awaited cleanup on ordinary errors and asynchronous drop cleanup on cancellation. AppState's add methods mutate and return without another await, so installing the guard immediately afterward does not introduce an extra cancellation window. Rollback must not infer ownership from before/after ID sets, which could accidentally include unrelated concurrent creations.

A deterministic CPU regression was proposed and then corrected after checking the complete startup path: restore a VNC client against a local peer that accepts but withholds its RFB greeting, wait until the resource is registered, cancel restore, and assert removal of that resource and scoped tasks while preserving a preexisting unrelated resource. VNC handshaking runs inline before create returns; POP3 greeting handling runs in a background task and would not provide the required startup barrier. EOF from the controlled VNC peer tests ordinary failure without relying on a released-port race. The runtime/server reviewer implemented the shared startup guard and `tests/startup_cancellation_test.rs`; no startup or save/load files were edited by this reviewer.

The earlier three core cross-review corrections were subsequently observed in the root-owned source: resource actions are required to be objects before mutation, easy wrappers validate the referenced protocol and refuse duplicate wrapping, and the atomic-file cleanup guard is created only after the staging file opens successfully. Final regression outcomes remain in the root's validation ledger.

## Final integration corrections

The root reported that the broad 46-feature native check passed, covering all changed client gates including SMB, Bluetooth, and USB. These are compilation results, not device or remote-service executions. Its first focused native test group also passed; the exact count and later SMB-specific run belong to the root's final validation ledger. Subsequent changes described below require the final focused rerun.

### SSH partial-write cancellation

Final review found that timing out `write_all` could leave a partial SSH frame on a reusable stream, and the timeout originally began only after acquiring the writer lock. The new `AgentWriter` stores its transport in an `Option`. A send takes ownership of that transport before awaiting any bytes and restores it only after the entire frame was written. Error, timeout, or cancellation drops the transport and leaves the writer permanently unusable. Production now uses `UnixStream::into_split` so dropping the owned write half half-closes the socket even while the read half exists.

One absolute deadline encloses both mutex acquisition and writing. A timeout while merely waiting for the lock does not poison the transport because it has not been taken or written. New duplex tests assert that internal timeouts and caller cancellation both produce EOF with only the original partial prefix, that a later command cannot append bytes, and that a lock-wait timeout leaves an untouched connection reusable. These raise `client_followup_test` to 18 tests.

### Eval harness classification and explicit model opt-in

Final read-only review found that output capture overflow was marked in `ProbeOutcome` but still passed through generic model-failure classification. A new pure `tests/eval/scoring.rs` helper now returns a harness `error` with `probe_output_limit` before checking expectations or diagnosing the model. `runner.rs` uses that helper when constructing `RunRecord`, preserving command/output/log evidence. No live suite is imported by the new `tests/eval_probe_classification_test.rs` target.

This reviewer added three pure scoring regressions: matching and nonmatching truncated output must both be harness errors; complete output must retain pass/fail behavior; and merely mentioning an action in rejected output must not count as execution. The runtime/server reviewer independently reviewed the handoff, added evidence-preservation checks, and added a fourth regression/fix for malformed expectation regexes being harness errors. Existing run-count/pass-rate accounting was preserved; the broader reporting denominator is not redesigned by this classification fix.

`tests/ollama_model_test.rs` now calls the common affirmative opt-in helper instead of treating any present `NETGET_USE_OLLAMA` value as consent. A search for the same concrete presence-only pattern under `tests` and `src` found no remaining matches. No model endpoint or live evaluation was invoked. These files were formatted and passed owned-path diff checks before handoff for centralized validation.

## Final centralized results

The final combined native run with 24 CPU-only features, including `smb-client`, passed **all 18 tests in `client_followup_test`** and all **22 tests in `client_review_regression_test`**. The pure `eval_probe_classification_test` passed **4/4**. The only failed target in that combined run was an unrelated pre-existing CLI timing fixture; after correcting its clock scope, all five CLI tests passed. The resulting native selection totals **324 passed across 41 targets**, with no outstanding failures. The final Clippy correctness/suspicious gate passed across all targets. The final 46-feature native library check passed; it compiles the changed optional protocol gates without exercising hardware or external services. Exact commands/logs are in the consolidated follow-up verification manifest. No client implementation changes were needed after these successful runs.
