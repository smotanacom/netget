# Server review — 2026-10-01

## Scope and evidence standard

This section review covers `src/server/**` and `tests/server/**`. At the initial inventory there were **434 Rust source files / 302,242 source lines**, **815 Rust test files / 189,321 test lines**, **166 immediate source directories**, and **seven shared source files**. `oracle/` contains planning documentation only; it is not an implemented or registered server. `http_common/` is shared HTTP infrastructure, while `usb/` contains multiple nested protocols.

Every directory and Rust file participated in structural and risk-pattern scans. Deep manual inspection concentrated on shared lifecycle/TLS code, framed stream reads and allocations, unchecked conversions, platform resource ownership, HTTP body limits, and certificate caching. **This is not a claim that every line received an independent manual proof or every protocol was exercised.** The table below identifies the complete scan surface and focused follow-ups.

No GPU computation, embedded model execution, real LLM endpoint, public protocol service, privileged raw socket, kernel device installation, deployment, or commit was used. New tests are pure CPU/in-process checks or a local TLS session with an incomplete message that cannot trigger a model call. Cargo validation is centralized by the root agent to avoid shared-target contention.

## Implemented changes

### S1. Certificate input validation returns errors instead of panicking

**Files:** `src/server/tls_cert_manager.rs`; `tests/server/tls/certificate_validation_test.rs`; TLS test registration and documentation.

The SAN builder previously called `try_into().unwrap()` on names coming from user/model startup parameters. A non-ASCII DNS SAN therefore panicked instead of returning the function's advertised `Result`. Certificate validity used `Duration::days(i64)` followed by unchecked date addition: extreme values overflowed duration construction or the supported date range, and nonpositive values created certificates with invalid or empty lifetimes. Startup SAN arrays silently dropped values that were not strings.

The implementation now collects fallible SAN conversions with contextual errors, requires a positive lifetime, checks the day-to-second multiplication, uses `OffsetDateTime::checked_add`, and rejects malformed SAN array elements with their index. Signing starts only after validation. The startup parameter description explains the positive/range requirement.

**Regression coverage:** non-ASCII SAN; zero, negative, minimum and maximum signed lifetimes; duration values that fit seconds but exceed the date range; a successful one-day wildcard certificate; numeric/null/object elements in the startup SAN array. Four CPU-only tests. DNS syntax beyond the existing rcgen IA5 conversion is not broadened by this change.

### S2. DoT partial bodies have a finite deadline

**Files:** `src/server/dot/mod.rs`; `tests/server/dot/connection_bounds_test.rs`; protocol/test documentation.

The server bounded the TLS handshake and DNS length-prefix read but awaited the DNS body without a timeout. A peer sending a two-byte length and a partial body could permanently hold a TLS session, task and connection permit. The five-minute between-query deadline was no longer being polled.

The body read now has a separate **10-second completion deadline** and a `decision=fail_closed_read_timeout` diagnostic. The normal between-query timeout remains 300 seconds. The timeout surrounds only the network body read; model/manual handling is outside it.

**Regression coverage:** establish a real loopback TLS connection, send a 12-byte declared DNS message with only one body byte, and require connection closure before a 15-second test deadline. No complete query exists and no model is invoked. Existing peer cleanup releases the cap slot when the connection handler exits.

### S3. Failed PTY setup releases both descriptors

**Files:** `src/server/pty/mod.rs`; `tests/server/pty/startup_cleanup_test.rs`; PTY test registration and documentation.

After `openpty`, the slave descriptor immediately became an `OwnedFd`, but the master stayed a raw integer until after raw-mode configuration, slave-path resolution, symlink creation and nonblocking setup. Any error before the conversion leaked the master. An existing ordinary file in `link_path` was a deterministic trigger.

The master becomes a `File` immediately after allocation, before every fallible setup step. Rust now drops both descriptors on all those error returns; successful startup passes the same owned master into `AsyncFd`.

**Regression coverage:** an isolated invocation of the test executable attempts 16 startups against a regular file, requires the documented rejection, checks the file contents survive, and compares `/dev/fd` counts before/after. The subprocess prevents unrelated concurrent tests from corrupting the count. No server loop or model starts.

### S4. Shared peer-command workers belong to the server

**Files:** `src/server/peer_support.rs`; `tests/server/tcp/peer_lifecycle_test.rs`; TCP test registration/documentation.

The shared peer-command helper detached its worker with `tokio::spawn`. Dropping the command sender at teardown only terminates a worker waiting on `recv`; it cannot cancel a worker already awaiting a blocked socket write or action. Such a worker retained the writer and state after server removal.

The helper now registers the worker with `AppState::register_server_task`. A short registration task preserves the synchronous API used by existing protocol callers. If server removal wins the registration race, the existing registration API immediately aborts the worker. This fixes server teardown centrally without changing protocol action behavior or signatures.

**Regression coverage:** a fake `AsyncWrite` announces its first poll and stays pending; the test removes the owning server and requires writer destruction and cancellation of the reply sender, while deliberately retaining the command sender. No network or model is involved.

**Reach:** the following 45 protocol modules invoke this helper: `amqp`, `beanstalkd`, `bitcoin`, `bolt`, `cassandra`, `db2`, `dc`, `dict`, `finger`, `ftp`, `gearman`, `gemini`, `gopher`, `ident`, `imap`, `irc`, `kafka`, `m3ua`, `memcached`, `modbus`, `mongodb`, `mqtt`, `mssql`, `nats`, `nntp`, `nostr`, `pop3`, `rdp`, `redis`, `reverse_shell`, `rtsp`, `smb`, `smtp`, `stomp`, `svn`, `tcp`, `telnet`, `tls`, `tor_relay`, `torrent_peer`, `torrent_tracker`, `vnc`, `whois`, `xmpp`, `zabbix`. A normal connection close during a blocked injected write, while its server continues running, remains a distinct lifecycle case and is not claimed as fixed here.

### S5. MITM leaf certificate cache has a cardinality bound

**Files:** `src/server/proxy/cert_cache.rs`; `tests/server/proxy/certificate_cache_bound_test.rs`; proxy test registration/documentation.

The per-domain cache retained certificates and private keys for 24 hours with hourly expired-entry cleanup but no entry cap. Unique peer-selected hostnames could grow it independently of the concurrent connection cap.

The cache now retains at most **1,024 certificate/key pairs**. A new domain at capacity evicts the oldest generated entry under the same write lock used for insertion, so concurrent misses cannot race above the bound. Replacing the same hostname uses its existing slot. Active TLS sessions own their copied certificate/key material and continue normally; an evicted domain generates a new pair on a future lookup. The policy is oldest-generation eviction, not LRU.

**Regression coverage:** fill the cache, prove a hit retains the original identity, insert beyond capacity, require oldest-pair regeneration, and prove the newest cached certificate still has the same matching key. An existing separate target, `tests/proxy_cert_cache_test.rs`, covers generation, key/certificate pairing, and normalization; the root agent has its validation command.

## Validation and execution status

- Individual changed Rust files were formatted with `rustfmt --edition 2021 --config skip_children=true`; successful.
- `git diff --check -- src/server tests/server`; successful at the local review checkpoint.
- Structural audit: every directory under `tests/server/` is declared in its parent module; no orphaned test directory found.
- Structural audit: no production `#[cfg(test)]` or inline `mod tests` found under `src/server/`.
- Every source section containing `TcpListener` also references `accept_bounded` or `ConnectionLimiter`; this is a lexical adoption check, not proof that every path retains its permit correctly.
- `src/server/oracle/` is the only source directory absent from `src/server/mod.rs`; it contains no Rust and explicitly describes planned work, so no registration was added.
- New test modules are explicitly declared in the existing per-protocol `mod.rs` files.
- Cargo outcomes are supplied by the root agent's centralized validation and the main report. They must not be inferred from the existence of these tests.

Requested targeted commands, through the repository wrapper with a CPU-only feature union:

```sh
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test server -- certificate_validation_test --test-threads=100
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test server -- an_incomplete_dns_body_releases_its_connection --test-threads=100
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test server -- rejected_link_path_does_not_leak_pty_descriptors --test-threads=100
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test server -- server_removal_cancels_a_blocked_peer_write --test-threads=100
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test server -- certificate_cache_bound_test --test-threads=100
./cargo-isolated.sh test --no-default-features --features tcp,tls,dot,pty,proxy --test proxy_cert_cache_test -- --test-threads=100
```

## Review method and checked risk clusters

The scans inspect allocation/read sites (`read_exact`, `read_to_end`, collection, `Vec` sizes), panicking operations (`unwrap`, `expect`, explicit panic macros), literal/indexed slices, unchecked shifts, task spawning and registration, listener-cap references, time arithmetic, unsafe blocks, filesystem mutation, and per-directory test/module presence. Lexical hits include comments and deliberately safe operations and are not counted as defects.

- **Shared infrastructure:** read `accept_bounded.rs`, `connection.rs`, `socket_helpers.rs`, `server_trait.rs`, `peer_support.rs`, `tls_cert_manager.rs`, and module gating. Existing semaphore admission/RAII, refusal-write timeout, bounded idle readers and busy guards were inspected. The peer worker and TLS defects above were the actionable changes.
- **Framed TCP/TLS:** inspected DoT, LLMNR, AMQP, SMB, MongoDB, ZooKeeper, Kafka, RDP, VNC and SVN framing/read/allocation sites. Existing maximum lengths and read-wrapper deadlines explain many superficially unbounded `read_exact` calls. DoT's body deadline was genuinely absent.
- **HTTP and HTTP-derived protocols:** scanned all `collect()` calls; inspected `http_common`, DoH, OpenAI, OpenAPI, etcd, WebDAV, Git, Mercurial and Ollama collection sites. Those inspected collections already use bounded bodies. No mechanical replacement was made merely because `.collect()` appeared.
- **Proxy:** inspected request/response handling, certificate generation/cache/TTL cleanup, and MITM body preview slicing. Byte-slice preview truncation uses `from_utf8_lossy` and is not a UTF-8 boundary panic. Certificate cache cardinality required the implemented bound.
- **UDP/datagrams:** inspected task-registration and buffer/read structure in DNS, CoAP, UDP, NTP, syslog and TFTP; scanned every datagram protocol's source. No live packet service or privileged socket was started.
- **Platform transports:** inspected PTY/FIFO ownership and relevant raw-socket conversion sites. Raw socket buffers are converted to slices from the returned initialized byte count; platform correctness is not claimed from static inspection. PTY ownership had a concrete error-path leak.
- **USB/BLE:** all nested source files were scanned. The USB transport guard and bounded allocation sites, mapped disk ownership and keyboard handler adaptations received focused checks. No USB device, BLE adapter, FIDO operation, disk-image mutation or privileged test was run.
- **Parser/codec inventory:** all code containing slicing, allocations and panic patterns was scanned; focused validation inspected NSQ, Gearman, OTLP decompression, VNC/RDP allocation limits and guarded wire slices. Existing recursive-parser limits and fuzz suites were inventoried but not fuzzed in this review.

## Remaining limits and follow-ups

1. Running all server E2E tests would start optional programs, devices and some real-model modes; the review intentionally requests exact CPU-safe regressions instead. Interoperability coverage, packet-dissection oracles, fuzz execution, timing at production load and protocol completeness remain at their previous evidence levels.
2. The QUIC startup adapter independently filters non-string SAN entries before calling the shared generator. Shared certificate generation is now panic-safe, but the stricter malformed-array rejection added to the common TLS extractor does not automatically reach this adapter. Consolidating its parameter extraction would remove that discrepancy.
3. The generic peer-command change fixes **server removal**. To bound a blocked peer write after only that peer is closed, the task needs a connection-scoped cancellation mechanism; command-channel closure alone is insufficient while a write is pending. Existing per-protocol write/deadline handling is not uniformly proven by this review.
4. Several protocol `CLAUDE.md` files contain historical contradictions (for example older unbounded-connection claims alongside later connection-bound sections). Touched claims relevant to fixes were updated; wholesale rewriting of all protocol histories was outside this code-focused pass.
5. Source-pattern absence is not safety proof. Arithmetic, parser and resource defects outside the focused paths may remain. No protocol maturity rating was raised on the strength of these scans.

## Complete per-directory coverage ledger

Counts below are the initial inventory. `cap` means an `accept_bounded` or `ConnectionLimiter` reference exists; `timeout` means a timeout-related token exists; `unsafe` counts explicit unsafe-block tokens. These are triage signals, not pass/fail ratings. Every listed section received the same structural/risk scan; focused notes identify extra manual work or direct fixes. Test counts are Rust files, not test cases. USB tests are split among `usb_*` directories; shared HTTP tests also live outside this tree.

| Source section | Rust files | Source lines | Same-name test Rust files | Scan signals | Focused review / disposition |
|---|---:|---:|---:|---|---|
| `amqp` | 3 | 4,214 | 6 | TCP, cap, timeout | Inspected frame allocation cap and outer timeout ownership. |
| `arp` | 2 | 1,006 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `beanstalkd` | 3 | 2,200 | 9 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `bgp` | 3 | 3,128 | 7 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `bitcoin` | 2 | 1,796 | 4 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `bluetooth_ble` | 2 | 2,111 | 5 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_battery` | 2 | 435 | 5 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_beacon` | 4 | 1,853 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_cycling` | 2 | 320 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_data_stream` | 2 | 321 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_environmental` | 2 | 337 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_file_transfer` | 2 | 335 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_gamepad` | 2 | 386 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_heart_rate` | 2 | 398 | 5 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_keyboard` | 2 | 497 | 5 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_mouse` | 2 | 471 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_presenter` | 2 | 554 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_proximity` | 2 | 334 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_remote` | 2 | 524 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_running` | 2 | 327 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_thermometer` | 2 | 322 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bluetooth_ble_weight_scale` | 2 | 318 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `bolt` | 5 | 3,170 | 10 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `bootp` | 2 | 963 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `can` | 4 | 2,460 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `cassandra` | 2 | 3,025 | 5 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `cdp` | 3 | 2,302 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `coap` | 3 | 2,005 | 6 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `couchdb` | 2 | 1,924 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `datalink` | 2 | 737 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `db2` | 3 | 1,509 | 4 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `dc` | 2 | 1,473 | 5 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `dhcp` | 2 | 1,206 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `dhcpv6` | 2 | 1,937 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `dict` | 3 | 1,794 | 8 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `dns` | 2 | 1,352 | 6 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `docker` | 3 | 2,221 | 6 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `doh` | 2 | 1,044 | 5 | TCP, cap, timeout, limited-body | Focused TLS/ALPN and limited HTTP body inspection. |
| `dot` | 2 | 822 | 5 | TCP, cap, timeout | Fixed missing DNS body deadline; real TLS partial-message regression. |
| `dynamo` | 2 | 879 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `eapol` | 3 | 2,870 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `elasticsearch` | 2 | 1,466 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `etcd` | 2 | 1,866 | 6 | TCP, cap, timeout, limited-body | Confirmed bounded request collection before gRPC frame parsing. |
| `finger` | 2 | 1,256 | 3 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `ftp` | 2 | 1,587 | 7 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `gearman` | 3 | 1,772 | 8 | TCP, cap, timeout | Inspected fixed-size header guarded conversions. |
| `gemini` | 3 | 1,528 | 9 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `git` | 4 | 1,900 | 3 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `gopher` | 2 | 1,047 | 4 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `grpc` | 2 | 1,891 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `gtp` | 3 | 4,354 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `hls` | 2 | 1,118 | 5 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `hsrp` | 3 | 2,126 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `http` | 2 | 1,325 | 7 | cap, timeout | Scanned shared HTTP delegation and TLS startup path. |
| `http2` | 4 | 1,753 | 5 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `http_common` | 3 | 937 | 0 | limited-body | Focused bounded HTTP body and response builder review; tests elsewhere. |
| `icmp` | 2 | 1,966 | 3 | timeout, unsafe=1 | Structural/risk scan; no demonstrated defect changed in this section. |
| `ident` | 2 | 1,226 | 3 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `igmp` | 2 | 1,240 | 3 | unsafe=2 | Structural/risk scan; no demonstrated defect changed in this section. |
| `imap` | 2 | 2,560 | 10 | cap, timeout, limited-body | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `ipp` | 2 | 1,660 | 6 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `ipsec` | 2 | 832 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `irc` | 3 | 1,581 | 7 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `isis` | 2 | 1,489 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `jsonrpc` | 2 | 1,151 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `kafka` | 2 | 2,599 | 5 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `kubernetes` | 4 | 2,573 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `ldap` | 2 | 2,854 | 6 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `lldp` | 3 | 2,700 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `llmnr` | 2 | 1,588 | 3 | TCP, cap, timeout | Inspected per-prefix and per-body read deadlines. |
| `m3ua` | 3 | 3,230 | 4 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `maven` | 2 | 1,390 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `mcp` | 3 | 2,259 | 5 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `mdns` | 2 | 836 | 3 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `memcached` | 3 | 2,065 | 8 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `mercurial` | 2 | 1,458 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `modbus` | 3 | 2,719 | 9 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `mongodb` | 2 | 1,772 | 9 | TCP, cap, timeout | Inspected minimum length, maximum frame and body deadline. |
| `mqtt` | 2 | 2,756 | 7 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `mssql` | 2 | 2,151 | 6 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `mysql` | 4 | 2,408 | 8 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `named_pipe` | 2 | 812 | 2 | timeout, unsafe=4 | Inspected FIFO validation, nonblocking ownership and write timeout. |
| `nats` | 2 | 2,202 | 4 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `ndp` | 3 | 3,217 | 3 | unsafe=1 | Inspected raw socket returned-length slice construction. |
| `netbios_ns` | 3 | 2,228 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `nfc` | 3 | 1,949 | 3 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `nfs` | 3 | 2,423 | 4 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `nntp` | 2 | 1,663 | 8 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `nostr` | 5 | 3,293 | 9 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `npm` | 2 | 1,244 | 5 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `nsq` | 3 | 2,692 | 9 | TCP, cap, timeout | Inspected count/length guarded frame allocations. |
| `ntp` | 2 | 1,144 | 5 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `oauth2` | 2 | 1,934 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `oci_registry` | 2 | 2,357 | 4 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `ollama` | 2 | 2,179 | 6 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `openai` | 2 | 1,085 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `openapi` | 2 | 1,618 | 7 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `openid` | 2 | 1,620 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `openvpn` | 8 | 3,212 | 4 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `oracle` | 0 | 0 | 0 | codec/action/transport scan | Planning documentation only; no Rust implementation to register/test. |
| `ospf` | 2 | 2,686 | 2 | unsafe=2 | Structural/risk scan; no demonstrated defect changed in this section. |
| `otlp` | 3 | 1,628 | 8 | cap, timeout, limited-body | Inspected bounded multi-member gzip output path. |
| `pop3` | 2 | 1,842 | 8 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `postgresql` | 2 | 1,746 | 7 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `prometheus` | 3 | 1,647 | 6 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `proxy` | 5 | 4,287 | 6 | TCP, cap, timeout | Fixed unbounded certificate cache; checked MITM/read paths. |
| `pty` | 2 | 760 | 2 | unsafe=8 | Fixed failed-startup master-fd leak; isolated descriptor regression. |
| `pypi` | 2 | 1,027 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `quic` | 2 | 1,372 | 3 | timeout | Inspected stream lifecycle/TLS parameter adapter; SAN-array discrepancy noted. |
| `radius` | 3 | 2,062 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `rawip` | 2 | 1,661 | 2 | unsafe=1 | Inspected initialized-length raw buffer conversion. |
| `rdp` | 2 | 1,045 | 4 | cap, timeout | Inspected TPKT size checks and idle-wrapped read path. |
| `redis` | 2 | 1,390 | 8 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `reverse_shell` | 2 | 1,102 | 4 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `rip` | 2 | 827 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `rss` | 2 | 1,054 | 4 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `rtp` | 3 | 1,602 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `rtsp` | 2 | 1,470 | 6 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `s3` | 2 | 1,637 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `saml_idp` | 2 | 1,128 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `saml_sp` | 2 | 1,268 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `sip` | 2 | 1,464 | 7 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `smb` | 4 | 4,521 | 11 | TCP, cap, timeout | Inspected header/body framing and bounded allocation paths. |
| `smtp` | 2 | 1,691 | 6 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `snmp` | 2 | 1,716 | 5 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `snowflake` | 2 | 1,597 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `socket_file` | 2 | 1,472 | 3 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `socks5` | 3 | 2,080 | 4 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `spark` | 2 | 1,025 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `sqs` | 2 | 891 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `ssdp` | 3 | 1,930 | 2 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `ssh` | 3 | 3,421 | 5 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `ssh_agent` | 2 | 2,036 | 5 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `stdio` | 2 | 712 | 2 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `stomp` | 3 | 2,193 | 5 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `stp` | 3 | 2,653 | 3 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `stun` | 2 | 1,239 | 6 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `svn` | 3 | 2,401 | 6 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `syslog` | 2 | 728 | 2 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `tcp` | 2 | 1,728 | 3 | cap, timeout | Shared peer-worker cancellation regression; existing lifecycle/bounds reviewed. |
| `telnet` | 2 | 1,369 | 6 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `tftp` | 2 | 1,902 | 4 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `tls` | 2 | 1,867 | 6 | TCP, cap, timeout | Shared certificate validation regression; checked queued-data/refusal paths. |
| `tor_relay` | 4 | 3,570 | 5 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `torrent_dht` | 2 | 1,243 | 4 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `torrent_peer` | 2 | 1,566 | 5 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `torrent_tracker` | 2 | 1,382 | 6 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `tuntap` | 3 | 3,559 | 3 | timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `turn` | 2 | 2,915 | 6 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `udp` | 2 | 843 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `usb` | 29 | 16,692 | 0 | cap, timeout, unsafe=1 | Nested transport/device files included; guards/ownership inspected; tests in usb_*. |
| `vault` | 3 | 1,483 | 6 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `vnc` | 2 | 2,250 | 5 | cap, timeout | Inspected clipboard allocation cap and CPU framebuffer rendering bounds. |
| `vrrp` | 3 | 2,839 | 3 | unsafe=3 | Structural/risk scan; no demonstrated defect changed in this section. |
| `webdav` | 2 | 1,718 | 5 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `webrtc` | 2 | 1,963 | 4 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `webrtc_signaling` | 2 | 1,552 | 6 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `websocket` | 2 | 3,126 | 4 | cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `whois` | 2 | 1,182 | 6 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `wireguard` | 2 | 1,427 | 2 | timeout | Structural/risk scan; no demonstrated defect changed in this section. |
| `wol` | 2 | 1,235 | 4 | codec/action/transport scan | Structural/risk scan; no demonstrated defect changed in this section. |
| `xmlrpc` | 2 | 1,661 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `xmpp` | 2 | 1,445 | 5 | cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `yarn` | 2 | 1,334 | 4 | cap, timeout, limited-body | Structural/risk scan; no demonstrated defect changed in this section. |
| `zabbix` | 3 | 1,182 | 9 | TCP, cap, timeout | Structural/risk scan; benefits from shared server-owned peer-command worker. |
| `zookeeper` | 2 | 1,912 | 4 | TCP, cap, timeout | Structural/risk scan; no demonstrated defect changed in this section. |

## Shared source files

| File | Review disposition |
|---|---|
| `accept_bounded.rs` | Inspected semaphore permits, refusal timeout, idle readers and activity guards; no change. |
| `connection.rs` | Inspected connection-ID parsing and basic counters; no change. |
| `mod.rs` | Checked module/test directory relationships and TLS feature gating; no change. |
| `peer_support.rs` | Fixed worker ownership; applies to 45 protocol callers. |
| `server_trait.rs` | Read common server trait; no change. |
| `socket_helpers.rs` | Inspected reusable TCP/UDP listeners and raw OSPF descriptor ownership; no change. |
| `tls_cert_manager.rs` | Fixed SAN and validity validation before key generation. |

## Test-only integration families

`tests/server/tor_integration/` and `tests/server/torrent_integration/` were included in the test-file structural inventory. Their network/integration scenarios were not executed. Nested USB suites (`usb_fido2`, `usb_keyboard`, `usb_mouse`, `usb_msc`, `usb_serial`, `usb_smartcard`) map to source subdirectories under `src/server/usb/`, rather than to immediate source siblings.
