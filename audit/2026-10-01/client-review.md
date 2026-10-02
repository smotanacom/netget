# Client, easy HTTP, and pipe review — 2026-10-01

## Scope and evidence

Inventory: 777 files across `src/client`, `src/easy`, `src/pipe`, and `tests/client`. Every inventoried text file was included in the static sweep. The sweep enumerated declarations, framing/allocation operations, unbounded reads and channels, unchecked arithmetic, process/filesystem access, task ownership, TODOs, and protocol/test documentation. Manual control-flow review concentrated on the changes and risks below. This is **not a claim that every line received equal-depth manual review or that every protocol was exercised**.

No GPU operations, model inference, model downloads, external protocol services, live LLM tests, hardware access, commits, or pushes were run by this reviewer. Rust builds/tests are centralized by the root agent; their authoritative results belong in the combined report.

## Implemented improvements

| Area | Defect | Change | Regression evidence |
|---|---|---|---|
| HTTP shared transport | Connection driver cleanup was after an await, so caller cancellation bypassed explicit cleanup. | An owning drop guard aborts HTTP/1 and HTTP/2 drivers on every exit. | In-memory request/prelude observed, then exchange cancelled and peer must reach EOF. |
| HTTP extension methods | Shared transport admitted only seven methods; WebDAV operations were refused. | Use HTTP Method parsing to admit valid extension-method tokens and preserve their casing; retain normalization of the seven previously supported methods. | PROPFIND, MKCOL, COPY, MOVE, LOCK, UNLOCK, mixed/lowercase custom tokens and standard-method normalization wire requests. |
| Easy HTTP Markdown | All emphasis/code delimiters became opening tags; code contents were reinterpreted; code fences could begin inside an unclosed list. | Paired escaped inline tags, literal unmatched syntax, code isolation, bounded recursion, close list before code. | Balanced tags, identifier underscores, escaped markup, unmatched delimiters, list/fence structure. |
| Pipe template memory | 64KiB decoded cap was applied after arbitrary template expansion and JSON serialization. | 1MiB intermediate representation cap enforced during append and streaming JSON serialization;64KiB payload cap retained. | Exact UTF-8/hex payload bound, expansion failure, oversized structured value and encoding substitution. |
| Pipe validation | An invalid explicit source silently defaulted to current server; invalid mapping values silently disappeared. | Only an absent source defaults; explicit source and every map value must validate. | Invalid string, negative, overflowing, null source; numeric encoding; valid contextual default. |
| Pipe allocation | Whole event payload was cloned before each dispatch batch. | Borrow the event during rendering. | Covered by existing and new mapping tests. |
| BitTorrent peer | Remote u32 length allocated up to4GiB before validation. | 8MiB inbound frame cap before allocation/read, with explicit keepalive handling. | Oversize/u32::MAX, exact cap, keepalive, next frame and truncated body. |
| VNC text | Remote failure/name/clipboard u32 lengths allocated without a cap. | One1MiB bounded text decoder used at all four allocation sites. | Over-limit/u32::MAX, exact cap, truncation. |
| VNC pixels | Raw rectangle allocated width*height*4 (up to~17GiB) then discarded; server pixel size was assumed. | Negotiate32bpp true color; discard via bounded scratch reads;256MiB per-rectangle cap. | Exact one-pixel read preserves following Bell, truncated pixels fail, maximum dimensions refused. |
| VNC framing/lifecycle | Unsupported encodings and malformed messages logged and continued with desynchronized stream. | End reader and clear command handle/status on framing error. | Decoder tests cover failure causes; end-to-end lifecycle requires further peer validation. |
| SSH agent | Raw socket chunks including length prefix were passed to parser expecting a message type; split/coalesced frames broke. | LengthDelimitedCodec strips prefix, preserves partial frames across cancellation and caps messages at1MiB. | Coalesced identities/success, cancelled partial header, over-limit/truncated frames. |
| FTP/POP3/NNTP/HTTP proxy | read_line could allocate without bound on an endless line. | Shared response reader limits lines to64KiB and treats partial EOF as framing error. | Exact cap, next-line boundary, too-long and incomplete line. |
| POP3/NNTP multiline | POP3 spun forever at EOF before terminator; NNTP reported truncated response as success; total response unbounded. | Require exact dot terminator,8MiB aggregate bound, undo dot stuffing and preserve content whitespace. | EOF failure, aggregate cap, dot stuffing, whitespace around dot, following response preserved. |
| POP3 lifecycle | Reader exit did not remove injected-command handle or consistently update status. | Cleanup after both normal and error return removes handle and sets final status. | Static control-flow review; full peer lifecycle not yet separately asserted. |
| HTTP CONNECT | 200 status plus malformed/EOF header block could be marked established; arbitrary number of short headers unbounded. | 64KiB aggregate head cap; incomplete/invalid headers return terminal error before tunnel event. | Shared reader tests; independent CONNECT exchange remains integration follow-up. |
| WebRTC ownership | Obsolete Arc::into_raw reference stored in JSON was released only by cleanup task, which stop could abort. | Remove the unused raw reference and unsafe reconstruction; live tasks retain proper Arc owners. | Static reference search; no hardware/network WebRTC session run. |
| Client LLM budget | Warning percentage multiplication used u32 and could overflow for a large configured limit. | Widen operands to u64 before multiplying. | Static arithmetic proof; no model calls. |

## Validation performed by this reviewer

- `rustfmt --edition 2021` on changed files, with `skip_children=true` for module roots to avoid touching unrelated files.
- `git diff --check` for owned source/tests.
- Created `tests/client_review_regression_test.rs`,22 CPU-only tests with the relevant features enabled. These use in-memory readers/duplex streams and constructed state, and never invoke a model.
- Centralized Cargo test/check results are pending at report creation; do not equate a test file existing with a passing test run.
- Feature set covering changed gated clients: `http,vnc,torrent-peer,ssh-agent,webrtc,ftp,nntp,pop3,http_proxy`. The helper/pipe tests are available without those protocol features.

## Material behavior changes and limits

-64KiB client text lines and CONNECT header blocks;8MiB POP3/NNTP accumulated response text;8MiB BitTorrent peer messages;1MiB SSH agent response frames and VNC text;256MiB raw VNC rectangles;1MiB intermediate pipe mapping and64KiB decoded payload. These deliberately refuse larger inputs rather than truncate and misrepresent success.
- Text response readers use the existing lossy UTF-8 line decoder; non-UTF-8 network text is represented with replacement characters instead of failing allocation/unicode decoding.
- Dot response bodies preserve indentation and trailing spaces, unstuff leading double dots, and use newline-separated retained text. Whitespace around a dot is ordinary content.
- Easy HTTP still implements a small Markdown subset, not CommonMark; no new dependency or full parser was introduced.
- New public framing/rendering helpers make production behavior directly testable from the required external `tests/` directory.

## Remaining findings and explicit gaps

1. **POP3 command correlation:** Existing code guesses multiline replies from the wording of `+OK` rather than tracking the command queue. USER/PASS/LIST/RETR replies can still be misclassified and wait for a dot that will never arrive. The safe complete fix needs response expectations queued atomically with writes from both the model and command channel; the bound/EOF fix does not claim to solve it.
2. **VNC authentication:** Type2 still echoes the challenge rather than performing DES; existing docs call this a placeholder. It was not expanded into a cryptographic implementation during the allocation/framing patch.
3. **Native HTTP-family body bounds are inconsistent:** `FetchClient` transport responses are bounded, but several reqwest-side `text()/json()` users still buffer without a protocol-specific cap (bitcoin, jsonrpc, MCP, HTTP/2, WebDAV, package metadata, OAuth/OIDC). A global cap can break intentional large streaming downloads, so policy must distinguish buffered model events from download streams.
4. **SMB client file reads:** `file.read_to_end` remains unbounded and synchronous while holding its client lock. Solving the !Send libsmbclient ownership and size/error contract deserves dedicated tests.
5. **WebRTC lifecycle beyond raw reference removal:** Library-internal tasks, callback ownership cycles, dropped queued channel messages, and connection close-on-abort need a real lifecycle test. Removing the unused raw Arc fixes that specific guaranteed leak only.
6. **SSH agent command injection:** Generic injected-command handler cannot execute Custom action results, although SSH agent verbs return them; it can report an Executed detail without sending the requested agent packet. The receive framing fix is separate from this existing command-channel gap.
7. **HTTP URL validation:** `parse_http_url` uses hyper Authority and `port_u16().unwrap_or(80)`; malformed explicit port handling and userinfo should be independently verified. No broad URL semantics rewrite was attempted.
8. **Protocol-specific timeouts and aggregate buffering:** Some protocols still depend on peer closure or external stop after beginning a partial message. This pass enforces allocation limits for the paths changed, not a universal idle/handshake deadline policy.
9. **Binary protocol narrowing:** Numerous action integers use `as u16/u32/u8`, particularly routing/datagram protocols. Existing validation varies; values were not indiscriminately clamped because that would silently alter packets.
10. **Documentation maturity:** Many historical client docs still describe placeholder tests, older test commands, nonexistent TLS, or stale limitations. The added sections document actual changes; they are not blanket verification of every historical claim.
11. **Hardware / unavailable environments:** Raw/link-layer packets, USB/NFC/Bluetooth, Tor/WireGuard privileged networking, real remote databases and registries, browser wasm and native TLS were static-only. No maturity rating was upgraded.

## Directory coverage ledger

Every listed directory was inventoried and scanned; `Focused` means manual control-flow review included concrete changed paths, `Focused static` means targeted source inspection without a patch, and `Sweep` means automated hazard/declaration inventory plus protocol documentation review, not equal-depth manual review.

| Client directory | Rust files | Rust lines | Test Rust files | Coverage |
|---|---:|---:|---:|---|
| `amqp` | 2 | 963 | 3 | Sweep |
| `arp` | 2 | 1296 | 3 | Sweep |
| `bgp` | 2 | 1623 | 5 | Focused static |
| `bitcoin` | 2 | 1301 | 4 | Sweep |
| `bluetooth` | 2 | 1828 | 3 | Sweep |
| `bootp` | 2 | 880 | 3 | Focused static |
| `cassandra` | 2 | 976 | 3 | Sweep |
| `coap` | 2 | 1672 | 4 | Focused static |
| `couchdb` | 2 | 2455 | 3 | Sweep |
| `datalink` | 2 | 1625 | 4 | Sweep |
| `dc` | 2 | 2721 | 3 | Focused static |
| `dhcp` | 2 | 1134 | 3 | Focused static |
| `dns` | 2 | 1061 | 3 | Sweep |
| `doh` | 2 | 1123 | 3 | Sweep |
| `dot` | 2 | 1102 | 3 | Sweep |
| `dynamodb` | 2 | 1699 | 3 | Sweep |
| `elasticsearch` | 2 | 1730 | 3 | Sweep |
| `etcd` | 2 | 1011 | 4 | Sweep |
| `finger` | 2 | 1624 | 2 | Sweep |
| `ftp` | 2 | 617 | 3 | Focused |
| `git` | 3 | 2618 | 5 | Focused static |
| `gopher` | 2 | 1635 | 2 | Sweep |
| `grpc` | 2 | 1752 | 3 | Sweep |
| `http` | 2 | 1220 | 6 | Focused |
| `http2` | 2 | 1116 | 4 | Sweep |
| `http3` | 2 | 1314 | 3 | Sweep |
| `http_fetch` | 2 | 931 | 0 | Focused |
| `http_proxy` | 2 | 1318 | 4 | Focused |
| `icmp` | 2 | 1385 | 4 | Sweep |
| `ident` | 2 | 1560 | 2 | Sweep |
| `igmp` | 2 | 947 | 3 | Sweep |
| `imap` | 2 | 1318 | 4 | Sweep |
| `ipp` | 2 | 1372 | 4 | Sweep |
| `irc` | 2 | 1149 | 4 | Sweep |
| `isis` | 2 | 770 | 4 | Sweep |
| `jsonrpc` | 2 | 1311 | 3 | Sweep |
| `kafka` | 2 | 2249 | 3 | Focused static |
| `kubernetes` | 2 | 1575 | 3 | Sweep |
| `ldap` | 2 | 1448 | 3 | Sweep |
| `llmnr` | 2 | 1861 | 2 | Sweep |
| `maven` | 2 | 1695 | 3 | Sweep |
| `mcp` | 2 | 1283 | 3 | Sweep |
| `mdns` | 2 | 1066 | 3 | Sweep |
| `memcached` | 3 | 1868 | 4 | Sweep |
| `mercurial` | 0 | 0 | 0 | Documentation-only directory; no implementation |
| `modbus` | 2 | 1232 | 4 | Sweep |
| `mongodb` | 2 | 1275 | 3 | Sweep |
| `mqtt` | 2 | 1262 | 4 | Sweep |
| `mssql` | 2 | 976 | 3 | Sweep |
| `mysql` | 2 | 1046 | 4 | Sweep |
| `nats` | 2 | 2199 | 2 | Focused static |
| `netbios_ns` | 3 | 2039 | 2 | Focused static |
| `nfc` | 3 | 2432 | 3 | Focused static |
| `nfs` | 2 | 1479 | 3 | Sweep |
| `nntp` | 2 | 1153 | 3 | Focused |
| `npm` | 2 | 1564 | 4 | Sweep |
| `ntp` | 2 | 802 | 3 | Focused static |
| `oauth2` | 2 | 2229 | 3 | Sweep |
| `ollama` | 2 | 1628 | 4 | Sweep |
| `openai` | 2 | 1613 | 4 | Sweep |
| `openapi` | 2 | 1484 | 4 | Sweep |
| `openidconnect` | 2 | 2396 | 3 | Sweep |
| `oracle` | 0 | 0 | 0 | Documentation-only directory; no implementation |
| `ospf` | 2 | 1526 | 3 | Sweep |
| `pop3` | 2 | 750 | 4 | Focused |
| `postgresql` | 2 | 1010 | 4 | Sweep |
| `pypi` | 2 | 1527 | 4 | Sweep |
| `radius` | 3 | 1872 | 4 | Focused static |
| `redis` | 3 | 1229 | 5 | Focused static |
| `rip` | 2 | 971 | 4 | Sweep |
| `rss` | 2 | 890 | 3 | Sweep |
| `s3` | 2 | 1608 | 3 | Sweep |
| `saml` | 2 | 1293 | 5 | Sweep |
| `sip` | 2 | 1643 | 4 | Focused static |
| `smb` | 2 | 1429 | 3 | Focused static |
| `smtp` | 2 | 1019 | 4 | Sweep |
| `snmp` | 2 | 1551 | 3 | Focused static |
| `socket_file` | 2 | 743 | 3 | Sweep |
| `socks5` | 2 | 858 | 4 | Sweep |
| `sqs` | 2 | 1391 | 3 | Sweep |
| `ssdp` | 2 | 1819 | 2 | Sweep |
| `ssh` | 2 | 1047 | 3 | Sweep |
| `ssh_agent` | 2 | 1035 | 3 | Focused |
| `stomp` | 2 | 1726 | 2 | Focused static |
| `stun` | 2 | 805 | 3 | Sweep |
| `svn` | 0 | 0 | 0 | Documentation-only directory; no implementation |
| `syslog` | 2 | 770 | 3 | Sweep |
| `tcp` | 2 | 640 | 2 | Sweep |
| `telnet` | 2 | 879 | 2 | Sweep |
| `tftp` | 2 | 1228 | 3 | Focused static |
| `tls` | 2 | 955 | 4 | Sweep |
| `tor` | 2 | 1370 | 6 | Sweep |
| `torrent_dht` | 2 | 924 | 3 | Sweep |
| `torrent_peer` | 2 | 926 | 3 | Focused |
| `torrent_tracker` | 2 | 1221 | 3 | Sweep |
| `turn` | 2 | 1610 | 4 | Focused static |
| `udp` | 2 | 917 | 3 | Sweep |
| `usb` | 2 | 1550 | 3 | Sweep |
| `vnc` | 2 | 1615 | 4 | Focused |
| `webdav` | 2 | 1472 | 3 | Sweep |
| `webrtc` | 2 | 1651 | 3 | Focused |
| `websocket` | 2 | 1537 | 4 | Sweep |
| `whois` | 2 | 915 | 3 | Sweep |
| `wireguard` | 2 | 1139 | 3 | Sweep |
| `xmlrpc` | 3 | 1423 | 3 | Focused static |
| `xmpp` | 2 | 1218 | 4 | Sweep |
| `zookeeper` | 2 | 1108 | 2 | Sweep |

Also covered: client shared `mod.rs`, `command_support.rs`, `llm_budget.rs`; all4 easy/pipe Rust modules. Existing `tests/client` files were inventoried and their protocol guidance read; broad external-peer suites were not run.

## Complete file inventory

The counts below permit checking coverage against repository contents. Source content is not duplicated in the report. New root regression target and this report are additional artifacts.

| File | Lines | SHA-256 prefix |
|---|---:|---|
| `src/client/amqp/CLAUDE.md` | 95 | `55847da44394` |
| `src/client/amqp/actions.rs` | 346 | `add5258344fa` |
| `src/client/amqp/mod.rs` | 617 | `c4d32b6c3f01` |
| `src/client/arp/CLAUDE.md` | 534 | `c3bed7551a7e` |
| `src/client/arp/actions.rs` | 459 | `9e348221515f` |
| `src/client/arp/mod.rs` | 837 | `c99b33a44672` |
| `src/client/bgp/CLAUDE.md` | 196 | `162ef2fa84d0` |
| `src/client/bgp/actions.rs` | 505 | `1768b9184e99` |
| `src/client/bgp/mod.rs` | 1118 | `38ab1299dd00` |
| `src/client/bitcoin/CLAUDE.md` | 364 | `a4b656ff7c2f` |
| `src/client/bitcoin/actions.rs` | 634 | `0659682c51d2` |
| `src/client/bitcoin/mod.rs` | 667 | `b567f48a8a2b` |
| `src/client/bluetooth/CLAUDE.md` | 359 | `98359bd8ca0a` |
| `src/client/bluetooth/actions.rs` | 638 | `f840455ee138` |
| `src/client/bluetooth/mod.rs` | 1190 | `ff1e0c3cf8c0` |
| `src/client/bootp/CLAUDE.md` | 507 | `4b199c334b06` |
| `src/client/bootp/actions.rs` | 306 | `defe6970b807` |
| `src/client/bootp/mod.rs` | 574 | `4731a63c23a1` |
| `src/client/cassandra/CLAUDE.md` | 388 | `d4769c6e7295` |
| `src/client/cassandra/actions.rs` | 352 | `3dc1a65870e7` |
| `src/client/cassandra/mod.rs` | 624 | `3f09157b7704` |
| `src/client/coap/CLAUDE.md` | 119 | `7f35ac3ce25b` |
| `src/client/coap/actions.rs` | 651 | `fa6ec549a7b9` |
| `src/client/coap/mod.rs` | 1021 | `805865c45044` |
| `src/client/command_support.rs` | 206 | `1957cc4f2afd` |
| `src/client/couchdb/CLAUDE.md` | 444 | `4069b5a2c0eb` |
| `src/client/couchdb/actions.rs` | 662 | `ccdf4b898de0` |
| `src/client/couchdb/mod.rs` | 1793 | `19ec384c7a30` |
| `src/client/datalink/CLAUDE.md` | 389 | `697115aa38b3` |
| `src/client/datalink/actions.rs` | 738 | `7972af58e2f8` |
| `src/client/datalink/mod.rs` | 887 | `25ce68d61e65` |
| `src/client/dc/CLAUDE.md` | 497 | `14131281d168` |
| `src/client/dc/actions.rs` | 791 | `742ffb48ac10` |
| `src/client/dc/mod.rs` | 1930 | `3d4afac8d164` |
| `src/client/dhcp/CLAUDE.md` | 394 | `a14d3d0c813b` |
| `src/client/dhcp/actions.rs` | 327 | `e0b320b84bd0` |
| `src/client/dhcp/mod.rs` | 807 | `1c4e23bd796a` |
| `src/client/dns/CLAUDE.md` | 405 | `716078b41ea3` |
| `src/client/dns/actions.rs` | 343 | `6902da612221` |
| `src/client/dns/mod.rs` | 718 | `fa973776c379` |
| `src/client/doh/CLAUDE.md` | 459 | `f838047732d4` |
| `src/client/doh/actions.rs` | 366 | `1c821a34b264` |
| `src/client/doh/mod.rs` | 757 | `f5f6617e0c27` |
| `src/client/dot/CLAUDE.md` | 333 | `924ddf3370f7` |
| `src/client/dot/actions.rs` | 362 | `e66b517bc358` |
| `src/client/dot/mod.rs` | 740 | `3e5adf27821a` |
| `src/client/dynamodb/CLAUDE.md` | 424 | `39937ee14c47` |
| `src/client/dynamodb/actions.rs` | 663 | `7f9ce75d60cc` |
| `src/client/dynamodb/mod.rs` | 1036 | `f5d3d617d5a8` |
| `src/client/elasticsearch/CLAUDE.md` | 355 | `54efa546a312` |
| `src/client/elasticsearch/actions.rs` | 575 | `f3c595b0709b` |
| `src/client/elasticsearch/mod.rs` | 1155 | `bdf3227746c1` |
| `src/client/etcd/CLAUDE.md` | 459 | `62c66f5db34e` |
| `src/client/etcd/actions.rs` | 375 | `a4601c424ed0` |
| `src/client/etcd/mod.rs` | 636 | `1b1905610de1` |
| `src/client/finger/CLAUDE.md` | 161 | `70b76b8f0b0d` |
| `src/client/finger/actions.rs` | 750 | `df5e0a80e3e0` |
| `src/client/finger/mod.rs` | 874 | `6f65921b62e0` |
| `src/client/ftp/CLAUDE.md` | 133 | `5438adce25a6` |
| `src/client/ftp/actions.rs` | 280 | `ed661508ebf9` |
| `src/client/ftp/mod.rs` | 337 | `9a14df059b89` |
| `src/client/git/CLAUDE.md` | 487 | `c555a7c5bbb7` |
| `src/client/git/actions.rs` | 802 | `12169b0d4357` |
| `src/client/git/mod.rs` | 1529 | `40e0731a0e62` |
| `src/client/git/sandbox.rs` | 287 | `57cf3262e2dd` |
| `src/client/gopher/CLAUDE.md` | 229 | `b1df10874512` |
| `src/client/gopher/actions.rs` | 675 | `e8c4dbf268bc` |
| `src/client/gopher/mod.rs` | 960 | `e7930ab6c143` |
| `src/client/grpc/CLAUDE.md` | 456 | `e94806dec67d` |
| `src/client/grpc/actions.rs` | 475 | `90b2317daafc` |
| `src/client/grpc/mod.rs` | 1277 | `3b3cf1cdcaa2` |
| `src/client/http/CLAUDE.md` | 333 | `1525312d0336` |
| `src/client/http/actions.rs` | 391 | `b811f93df9da` |
| `src/client/http/mod.rs` | 829 | `da2d0bc59812` |
| `src/client/http2/CLAUDE.md` | 318 | `2c08dcdf921d` |
| `src/client/http2/actions.rs` | 366 | `f82f64330125` |
| `src/client/http2/mod.rs` | 750 | `206e17aa9586` |
| `src/client/http3/CLAUDE.md` | 459 | `087b3c637759` |
| `src/client/http3/actions.rs` | 426 | `64bc2e9f0950` |
| `src/client/http3/mod.rs` | 888 | `67c3689b3ce6` |
| `src/client/http_fetch/mod.rs` | 559 | `89ae049dbf5d` |
| `src/client/http_fetch/transport.rs` | 372 | `59a5bbbc82db` |
| `src/client/http_proxy/CLAUDE.md` | 226 | `c625d50e7af7` |
| `src/client/http_proxy/actions.rs` | 516 | `6e7efa0e8a9d` |
| `src/client/http_proxy/mod.rs` | 802 | `881a6c4cec1d` |
| `src/client/icmp/CLAUDE.md` | 396 | `cd0d94cf7909` |
| `src/client/icmp/actions.rs` | 565 | `f4da7af1c424` |
| `src/client/icmp/mod.rs` | 820 | `fe4a5bdf73ab` |
| `src/client/ident/CLAUDE.md` | 188 | `fb649ce7bc3e` |
| `src/client/ident/actions.rs` | 540 | `3c2021aac9e6` |
| `src/client/ident/mod.rs` | 1020 | `785771bc27da` |
| `src/client/igmp/CLAUDE.md` | 322 | `d0f4b7e68166` |
| `src/client/igmp/actions.rs` | 384 | `c602076fca92` |
| `src/client/igmp/mod.rs` | 563 | `5bccb4a10a8e` |
| `src/client/imap/CLAUDE.md` | 297 | `dfc0d0ed43eb` |
| `src/client/imap/actions.rs` | 543 | `0102c53b5a28` |
| `src/client/imap/mod.rs` | 775 | `6647d3db269c` |
| `src/client/ipp/CLAUDE.md` | 219 | `74d8b1d45cf0` |
| `src/client/ipp/actions.rs` | 470 | `378f49052b3e` |
| `src/client/ipp/mod.rs` | 902 | `c55f0eec5d55` |
| `src/client/irc/CLAUDE.md` | 250 | `75778674800d` |
| `src/client/irc/actions.rs` | 535 | `7f3aabd2ba49` |
| `src/client/irc/mod.rs` | 614 | `e9e9137eec71` |
| `src/client/isis/CLAUDE.md` | 272 | `6f9622649f7b` |
| `src/client/isis/actions.rs` | 269 | `e0c63b26d41a` |
| `src/client/isis/mod.rs` | 501 | `b6119b70b5c2` |
| `src/client/jsonrpc/CLAUDE.md` | 395 | `b89bc0f22a88` |
| `src/client/jsonrpc/actions.rs` | 367 | `139fa8b8008d` |
| `src/client/jsonrpc/mod.rs` | 944 | `e040b5cd7208` |
| `src/client/kafka/CLAUDE.md` | 194 | `7d7e1db1fee1` |
| `src/client/kafka/actions.rs` | 845 | `754d87838f52` |
| `src/client/kafka/mod.rs` | 1404 | `4ec305737cfb` |
| `src/client/kubernetes/CLAUDE.md` | 414 | `bcad10a7111c` |
| `src/client/kubernetes/actions.rs` | 642 | `39afa441dde6` |
| `src/client/kubernetes/mod.rs` | 933 | `f1d13659f64e` |
| `src/client/ldap/CLAUDE.md` | 288 | `bd03c1c8ef46` |
| `src/client/ldap/actions.rs` | 624 | `5d3bffee7f5d` |
| `src/client/ldap/mod.rs` | 824 | `50fafc7d2e0a` |
| `src/client/llm_budget.rs` | 210 | `5a7743a989b2` |
| `src/client/llmnr/CLAUDE.md` | 240 | `a8a4faa31991` |
| `src/client/llmnr/actions.rs` | 783 | `f447283e2223` |
| `src/client/llmnr/mod.rs` | 1078 | `882c5e72f614` |
| `src/client/maven/CLAUDE.md` | 278 | `b970bc835f43` |
| `src/client/maven/actions.rs` | 580 | `5b627d9e1989` |
| `src/client/maven/mod.rs` | 1115 | `2ed1ddbc8ae0` |
| `src/client/mcp/CLAUDE.md` | 394 | `82759d24e060` |
| `src/client/mcp/actions.rs` | 445 | `d769a6a4fffe` |
| `src/client/mcp/mod.rs` | 838 | `d6a4c987d716` |
| `src/client/mdns/CLAUDE.md` | 346 | `1c0a9ecc5aba` |
| `src/client/mdns/actions.rs` | 348 | `dcf327dc771f` |
| `src/client/mdns/mod.rs` | 718 | `1b851c666bf5` |
| `src/client/memcached/CLAUDE.md` | 107 | `13b3ba505604` |
| `src/client/memcached/actions.rs` | 812 | `c39770949164` |
| `src/client/memcached/mod.rs` | 556 | `f6961b18ecf0` |
| `src/client/memcached/wire.rs` | 500 | `61c4b840a378` |
| `src/client/mercurial/CLAUDE.md` | 1100 | `42dde3360bac` |
| `src/client/mod.rs` | 626 | `82354860ba56` |
| `src/client/modbus/CLAUDE.md` | 106 | `6742118d48ea` |
| `src/client/modbus/actions.rs` | 611 | `3aec10801077` |
| `src/client/modbus/mod.rs` | 621 | `0b6fcc8098f9` |
| `src/client/mongodb/CLAUDE.md` | 491 | `3b127c6453fd` |
| `src/client/mongodb/actions.rs` | 516 | `ee4f07e8709c` |
| `src/client/mongodb/mod.rs` | 759 | `f728b5c97e2d` |
| `src/client/mqtt/CLAUDE.md` | 235 | `ac700d4d123c` |
| `src/client/mqtt/actions.rs` | 511 | `fe03993ce9a8` |
| `src/client/mqtt/mod.rs` | 751 | `63c0ee4a3657` |
| `src/client/mssql/CLAUDE.md` | 296 | `af24118e39a2` |
| `src/client/mssql/actions.rs` | 311 | `acc49f489fca` |
| `src/client/mssql/mod.rs` | 665 | `bb4e5d3db694` |
| `src/client/mysql/CLAUDE.md` | 471 | `fc7aff3e4477` |
| `src/client/mysql/actions.rs` | 393 | `428e96a38f0f` |
| `src/client/mysql/mod.rs` | 653 | `0a179b7494b3` |
| `src/client/nats/CLAUDE.md` | 206 | `477a7f427a09` |
| `src/client/nats/actions.rs` | 1109 | `cdc0c8355958` |
| `src/client/nats/mod.rs` | 1090 | `5157cfc99cf5` |
| `src/client/netbios_ns/CLAUDE.md` | 214 | `71022a6f66e2` |
| `src/client/netbios_ns/actions.rs` | 674 | `dec3d6c0eaa3` |
| `src/client/netbios_ns/mod.rs` | 933 | `bafbc5d9202b` |
| `src/client/netbios_ns/wire.rs` | 432 | `497a80ef4bbe` |
| `src/client/nfc/CLAUDE.md` | 241 | `1b58f02b4864` |
| `src/client/nfc/actions.rs` | 689 | `1dfbd928d7c1` |
| `src/client/nfc/mod.rs` | 1043 | `be373a10a188` |
| `src/client/nfc/ndef.rs` | 700 | `d45d086de926` |
| `src/client/nfs/CLAUDE.md` | 533 | `3d87703fbb2e` |
| `src/client/nfs/actions.rs` | 487 | `4f8fc70f12ae` |
| `src/client/nfs/mod.rs` | 992 | `e8d933b94b58` |
| `src/client/nntp/CLAUDE.md` | 243 | `091b46dbbf47` |
| `src/client/nntp/actions.rs` | 508 | `b5337280f14e` |
| `src/client/nntp/mod.rs` | 645 | `fd89103a277c` |
| `src/client/npm/CLAUDE.md` | 441 | `fa145c4edd8d` |
| `src/client/npm/actions.rs` | 487 | `44429b1d4c77` |
| `src/client/npm/mod.rs` | 1077 | `1164e697716d` |
| `src/client/ntp/CLAUDE.md` | 275 | `0f0a094f2db6` |
| `src/client/ntp/actions.rs` | 259 | `da902c085598` |
| `src/client/ntp/mod.rs` | 543 | `0a0f30758f8c` |
| `src/client/oauth2/CLAUDE.md` | 262 | `96344a826532` |
| `src/client/oauth2/actions.rs` | 610 | `ae3d0c00e704` |
| `src/client/oauth2/mod.rs` | 1619 | `235bbe08553f` |
| `src/client/ollama/CLAUDE.md` | 371 | `1e940e804ad3` |
| `src/client/ollama/actions.rs` | 459 | `5d0d19bf7709` |
| `src/client/ollama/mod.rs` | 1169 | `c63741e73722` |
| `src/client/openai/CLAUDE.md` | 342 | `34c7de4c75c2` |
| `src/client/openai/actions.rs` | 493 | `638c4f7c9170` |
| `src/client/openai/mod.rs` | 1120 | `999dea179001` |
| `src/client/openapi/CLAUDE.md` | 401 | `0197e3ae92ca` |
| `src/client/openapi/actions.rs` | 526 | `db264ac06d2e` |
| `src/client/openapi/mod.rs` | 958 | `62c3d04bf5aa` |
| `src/client/openidconnect/CLAUDE.md` | 494 | `4130264bb2b9` |
| `src/client/openidconnect/actions.rs` | 609 | `2d251edcb2a9` |
| `src/client/openidconnect/mod.rs` | 1787 | `d046ad3cc266` |
| `src/client/oracle/CLAUDE.md` | 917 | `7c4afb68aa20` |
| `src/client/ospf/CLAUDE.md` | 698 | `680331cebef3` |
| `src/client/ospf/actions.rs` | 637 | `ee2c5cf313b0` |
| `src/client/ospf/mod.rs` | 889 | `127a32febbaf` |
| `src/client/pop3/CLAUDE.md` | 237 | `e9109138362f` |
| `src/client/pop3/actions.rs` | 294 | `aa85915ac420` |
| `src/client/pop3/mod.rs` | 456 | `f8028b9fec4b` |
| `src/client/postgresql/CLAUDE.md` | 288 | `d970fe6b5a16` |
| `src/client/postgresql/actions.rs` | 383 | `28b2ebf309c6` |
| `src/client/postgresql/mod.rs` | 627 | `76bbd61727a2` |
| `src/client/pypi/CLAUDE.md` | 315 | `5ddc3e82a706` |
| `src/client/pypi/actions.rs` | 488 | `6a7b3b473f5c` |
| `src/client/pypi/mod.rs` | 1039 | `9aa0ba0141ab` |
| `src/client/radius/CLAUDE.md` | 112 | `970731861992` |
| `src/client/radius/actions.rs` | 836 | `741b821c9cdb` |
| `src/client/radius/mod.rs` | 711 | `865a6f6133d4` |
| `src/client/radius/wire.rs` | 325 | `aeb92046207b` |
| `src/client/redis/CLAUDE.md` | 211 | `0e3922764d6d` |
| `src/client/redis/actions.rs` | 315 | `7b53b670f809` |
| `src/client/redis/mod.rs` | 465 | `85f71395962a` |
| `src/client/redis/resp.rs` | 449 | `fb2125d39a12` |
| `src/client/response_reader.rs` | 76 | `0676653e426c` |
| `src/client/rip/CLAUDE.md` | 294 | `4af63c56e806` |
| `src/client/rip/actions.rs` | 299 | `8e52d877208e` |
| `src/client/rip/mod.rs` | 672 | `9d46d493dca9` |
| `src/client/rss/CLAUDE.md` | 120 | `fef2a426e9d4` |
| `src/client/rss/actions.rs` | 317 | `acb8a2c9cc6d` |
| `src/client/rss/mod.rs` | 573 | `76c49be88a7c` |
| `src/client/s3/CLAUDE.md` | 384 | `24bad05886be` |
| `src/client/s3/actions.rs` | 711 | `b577c22d33db` |
| `src/client/s3/mod.rs` | 897 | `cd1a1a7aa56d` |
| `src/client/saml/CLAUDE.md` | 293 | `3cc4319315ab` |
| `src/client/saml/actions.rs` | 371 | `a0bda9510654` |
| `src/client/saml/mod.rs` | 922 | `336c128c2cdd` |
| `src/client/sip/CLAUDE.md` | 572 | `fb269bc28782` |
| `src/client/sip/actions.rs` | 663 | `957354224dee` |
| `src/client/sip/mod.rs` | 980 | `c2174a8dacb1` |
| `src/client/smb/CLAUDE.md` | 389 | `662dbaaaf832` |
| `src/client/smb/actions.rs` | 598 | `f82b3168a6d6` |
| `src/client/smb/mod.rs` | 831 | `2d07cdf487d2` |
| `src/client/smtp/CLAUDE.md` | 248 | `a349b7ae7656` |
| `src/client/smtp/actions.rs` | 416 | `90fb70ce6ddf` |
| `src/client/smtp/mod.rs` | 603 | `57f1401f901e` |
| `src/client/snmp/CLAUDE.md` | 493 | `f8ce5109b623` |
| `src/client/snmp/README.md` | 109 | `979b8b1f944b` |
| `src/client/snmp/actions.rs` | 482 | `1877ae1f6b3a` |
| `src/client/snmp/mod.rs` | 1069 | `fed6c7da68b9` |
| `src/client/socket_file/CLAUDE.md` | 247 | `2a9d3a6cb2b3` |
| `src/client/socket_file/actions.rs` | 378 | `efaef09878d0` |
| `src/client/socket_file/mod.rs` | 365 | `777e481b62dd` |
| `src/client/socks5/CLAUDE.md` | 357 | `326e261b38c1` |
| `src/client/socks5/actions.rs` | 471 | `ba0f12e4e018` |
| `src/client/socks5/mod.rs` | 387 | `c8095e5a4045` |
| `src/client/sqs/CLAUDE.md` | 343 | `fd88e5b8c2fd` |
| `src/client/sqs/actions.rs` | 497 | `a53864029a40` |
| `src/client/sqs/mod.rs` | 894 | `5f39a5d4fe85` |
| `src/client/ssdp/CLAUDE.md` | 192 | `297c55efff47` |
| `src/client/ssdp/actions.rs` | 790 | `ff62c063b78a` |
| `src/client/ssdp/mod.rs` | 1029 | `5797412887e9` |
| `src/client/ssh/CLAUDE.md` | 356 | `69c0c188c1c2` |
| `src/client/ssh/actions.rs` | 380 | `0491e7796224` |
| `src/client/ssh/mod.rs` | 667 | `86d1c79c72ba` |
| `src/client/ssh_agent/CLAUDE.md` | 247 | `224a1e8283bc` |
| `src/client/ssh_agent/actions.rs` | 402 | `f43c5c0ef565` |
| `src/client/ssh_agent/mod.rs` | 633 | `ea9ad85441c7` |
| `src/client/stomp/CLAUDE.md` | 292 | `a82260e046bb` |
| `src/client/stomp/actions.rs` | 886 | `d18a7f219bca` |
| `src/client/stomp/mod.rs` | 840 | `e8dd11d154b3` |
| `src/client/stun/CLAUDE.md` | 289 | `015fa35176be` |
| `src/client/stun/actions.rs` | 292 | `fa94af973964` |
| `src/client/stun/mod.rs` | 513 | `1b032568f3dd` |
| `src/client/svn/CLAUDE.md` | 1034 | `b460ea172ce7` |
| `src/client/syslog/CLAUDE.md` | 216 | `963fe92d30e9` |
| `src/client/syslog/actions.rs` | 312 | `14e183bb5f83` |
| `src/client/syslog/mod.rs` | 458 | `2fc87502de94` |
| `src/client/tcp/CLAUDE.md` | 120 | `8cd15371423d` |
| `src/client/tcp/actions.rs` | 300 | `3e34d37df100` |
| `src/client/tcp/mod.rs` | 340 | `e1cbb54dbf01` |
| `src/client/telnet/CLAUDE.md` | 255 | `5a52935b0c16` |
| `src/client/telnet/actions.rs` | 342 | `3d3a6f9c6d66` |
| `src/client/telnet/mod.rs` | 537 | `aa531ca38680` |
| `src/client/tftp/CLAUDE.md` | 121 | `9057d7b6253d` |
| `src/client/tftp/actions.rs` | 480 | `85e0dd5ab234` |
| `src/client/tftp/mod.rs` | 748 | `2533ea046f62` |
| `src/client/tls/CLAUDE.md` | 242 | `079ce26b5b85` |
| `src/client/tls/actions.rs` | 346 | `14a3f3482107` |
| `src/client/tls/mod.rs` | 609 | `13c7e6b91add` |
| `src/client/tor/CLAUDE.md` | 487 | `ba1f083b3f05` |
| `src/client/tor/actions.rs` | 558 | `d9589c57483b` |
| `src/client/tor/mod.rs` | 812 | `736a9ee0effa` |
| `src/client/torrent_dht/CLAUDE.md` | 152 | `a1758e8b753f` |
| `src/client/torrent_dht/actions.rs` | 360 | `284a7f3dbeee` |
| `src/client/torrent_dht/mod.rs` | 564 | `78126d284bbc` |
| `src/client/torrent_peer/CLAUDE.md` | 177 | `468800fe2313` |
| `src/client/torrent_peer/actions.rs` | 394 | `3a781d1ee552` |
| `src/client/torrent_peer/mod.rs` | 532 | `3ed05fefda78` |
| `src/client/torrent_tracker/CLAUDE.md` | 195 | `f3493b1530dd` |
| `src/client/torrent_tracker/actions.rs` | 316 | `741020dc2ac1` |
| `src/client/torrent_tracker/mod.rs` | 905 | `8b2a2ab6706e` |
| `src/client/turn/CLAUDE.md` | 417 | `3da8ee3c2bfb` |
| `src/client/turn/actions.rs` | 504 | `a488a4e72561` |
| `src/client/turn/mod.rs` | 1106 | `a9ca7259473a` |
| `src/client/udp/CLAUDE.md` | 312 | `fd26b728faff` |
| `src/client/udp/actions.rs` | 336 | `083b9d8ea7e1` |
| `src/client/udp/mod.rs` | 581 | `285332140133` |
| `src/client/usb/CLAUDE.md` | 245 | `59b5c8eb3631` |
| `src/client/usb/actions.rs` | 624 | `369ff4340999` |
| `src/client/usb/mod.rs` | 926 | `8efbf806b975` |
| `src/client/vnc/CLAUDE.md` | 285 | `a2bd8b73c544` |
| `src/client/vnc/actions.rs` | 569 | `056d012f8667` |
| `src/client/vnc/mod.rs` | 1046 | `20d833b6c08d` |
| `src/client/webdav/CLAUDE.md` | 292 | `0631dd4e344e` |
| `src/client/webdav/actions.rs` | 642 | `292782e45cd6` |
| `src/client/webdav/mod.rs` | 830 | `81323c27f96e` |
| `src/client/webrtc/CLAUDE.md` | 554 | `ff8d484da64b` |
| `src/client/webrtc/actions.rs` | 510 | `91347dac2da8` |
| `src/client/webrtc/mod.rs` | 1141 | `2c5c1bfc1686` |
| `src/client/websocket/CLAUDE.md` | 132 | `fc8f872a3044` |
| `src/client/websocket/actions.rs` | 627 | `1b0b6283e25a` |
| `src/client/websocket/mod.rs` | 910 | `0beb4692b73d` |
| `src/client/whois/CLAUDE.md` | 301 | `079ef473bf17` |
| `src/client/whois/actions.rs` | 315 | `588b828d44d4` |
| `src/client/whois/mod.rs` | 600 | `fdde3e4fb78e` |
| `src/client/wireguard/CLAUDE.md` | 441 | `ec8bbfde3b84` |
| `src/client/wireguard/actions.rs` | 350 | `46eb4c4c9dd7` |
| `src/client/wireguard/mod.rs` | 789 | `5decfb8accb6` |
| `src/client/xmlrpc/CLAUDE.md` | 360 | `767908dc71c2` |
| `src/client/xmlrpc/actions.rs` | 308 | `854184804924` |
| `src/client/xmlrpc/mod.rs` | 831 | `755d60d838a7` |
| `src/client/xmlrpc/response_guard.rs` | 284 | `c17a9337281f` |
| `src/client/xmpp/CLAUDE.md` | 425 | `5308c06c1a45` |
| `src/client/xmpp/actions.rs` | 418 | `f4f99baef5cb` |
| `src/client/xmpp/mod.rs` | 800 | `56c4e3183ddd` |
| `src/client/zookeeper/CLAUDE.md` | 145 | `97a1ed0cda22` |
| `src/client/zookeeper/actions.rs` | 497 | `f9c0667aab9e` |
| `src/client/zookeeper/mod.rs` | 611 | `bb7af307e879` |
| `src/easy/http/actions.rs` | 439 | `cbc3cc61e434` |
| `src/easy/http/mod.rs` | 3 | `c53c9c308961` |
| `src/easy/mod.rs` | 11 | `32794a983014` |
| `src/pipe/mod.rs` | 419 | `338b87c3f35a` |
| `tests/client/amqp/CLAUDE.md` | 80 | `1b5074392817` |
| `tests/client/amqp/command_channel_test.rs` | 205 | `c4149f0b717f` |
| `tests/client/amqp/e2e_test.rs` | 176 | `2aecd3f8d6b3` |
| `tests/client/amqp/mod.rs` | 6 | `1c17539b4c38` |
| `tests/client/arp/CLAUDE.md` | 301 | `0ddfebf9c6b6` |
| `tests/client/arp/command_channel_test.rs` | 461 | `733a08664a4b` |
| `tests/client/arp/e2e_test.rs` | 255 | `8ce013acdd46` |
| `tests/client/arp/mod.rs` | 4 | `6077aff58afe` |
| `tests/client/bgp/CLAUDE.md` | 114 | `61ac3462618c` |
| `tests/client/bgp/command_channel_test.rs` | 229 | `005b42940c31` |
| `tests/client/bgp/e2e_test.rs` | 248 | `0a19a60d02f0` |
| `tests/client/bgp/hold_timer_test.rs` | 374 | `b4ca8084ffa8` |
| `tests/client/bgp/mod.rs` | 8 | `1306af247882` |
| `tests/client/bgp/update_reply_test.rs` | 408 | `b1366b55f36f` |
| `tests/client/bitcoin/CLAUDE.md` | 61 | `076a44df422b` |
| `tests/client/bitcoin/command_channel_test.rs` | 227 | `8cd995e21273` |
| `tests/client/bitcoin/e2e_test.rs` | 244 | `71ff550d86fe` |
| `tests/client/bitcoin/mod.rs` | 19 | `92546a276616` |
| `tests/client/bitcoin/rpc_auth_test.rs` | 256 | `15cd16ecd6d8` |
| `tests/client/bluetooth/CLAUDE.md` | 381 | `982704a05767` |
| `tests/client/bluetooth/command_channel_test.rs` | 210 | `1c7bb2635d80` |
| `tests/client/bluetooth/e2e_test.rs` | 301 | `d2abc5a3ac74` |
| `tests/client/bluetooth/mod.rs` | 4 | `7fca17c3681c` |
| `tests/client/bootp/CLAUDE.md` | 436 | `52ee31c28527` |
| `tests/client/bootp/command_channel_test.rs` | 169 | `63f71fbaebb3` |
| `tests/client/bootp/e2e_test.rs` | 265 | `609884d05edf` |
| `tests/client/bootp/mod.rs` | 4 | `dcd712b08d08` |
| `tests/client/cassandra/CLAUDE.md` | 190 | `1dfdb5a9a880` |
| `tests/client/cassandra/command_channel_test.rs` | 213 | `a722beb8d595` |
| `tests/client/cassandra/e2e_test.rs` | 499 | `48f556e039b7` |
| `tests/client/cassandra/mod.rs` | 4 | `f01114a94837` |
| `tests/client/coap/CLAUDE.md` | 46 | `dbcf2fa1dd29` |
| `tests/client/coap/mod.rs` | 6 | `3ce3ab91a735` |
| `tests/client/coap/real_server_test.rs` | 274 | `556c7b381dea` |
| `tests/client/coap/request_test.rs` | 51 | `a44c2aaa829a` |
| `tests/client/coap/transport_test.rs` | 429 | `e135de0d6c43` |
| `tests/client/couchdb/CLAUDE.md` | 140 | `0c2e2e6b823a` |
| `tests/client/couchdb/command_channel_test.rs` | 253 | `eacd5ab66af4` |
| `tests/client/couchdb/e2e_test.rs` | 580 | `a3c62cb44b89` |
| `tests/client/couchdb/mod.rs` | 7 | `7044299403d9` |
| `tests/client/datalink/CLAUDE.md` | 132 | `496a82253752` |
| `tests/client/datalink/action_test.rs` | 712 | `26ad2ac11eee` |
| `tests/client/datalink/command_channel_test.rs` | 345 | `c8617db97d82` |
| `tests/client/datalink/e2e_test.rs` | 379 | `c793fe26ba9b` |
| `tests/client/datalink/mod.rs` | 6 | `c410beba88b4` |
| `tests/client/dc/CLAUDE.md` | 296 | `e38204c223bc` |
| `tests/client/dc/command_channel_test.rs` | 174 | `14dad331849a` |
| `tests/client/dc/e2e_test.rs` | 223 | `3397959e9c9e` |
| `tests/client/dc/mod.rs` | 4 | `35ab072b7941` |
| `tests/client/dhcp/CLAUDE.md` | 329 | `979f624e9c10` |
| `tests/client/dhcp/command_channel_test.rs` | 173 | `b94a38c5009f` |
| `tests/client/dhcp/e2e_test.rs` | 385 | `c458fcdbc68b` |
| `tests/client/dhcp/mod.rs` | 5 | `51820ccf8806` |
| `tests/client/dns/CLAUDE.md` | 284 | `5252fdbbf651` |
| `tests/client/dns/command_channel_test.rs` | 210 | `8c734dfb2154` |
| `tests/client/dns/e2e_test.rs` | 328 | `c40a60c52cc0` |
| `tests/client/dns/mod.rs` | 5 | `97ab2d4d7d72` |
| `tests/client/doh/CLAUDE.md` | 315 | `f1c2bf48af2d` |
| `tests/client/doh/command_channel_test.rs` | 243 | `dfeae695a985` |
| `tests/client/doh/e2e_test.rs` | 555 | `e33d0f2d1c8e` |
| `tests/client/doh/mod.rs` | 4 | `4421ae94f6b4` |
| `tests/client/dot/CLAUDE.md` | 262 | `e0394e425a55` |
| `tests/client/dot/command_channel_test.rs` | 168 | `50c3060c90ec` |
| `tests/client/dot/e2e_test.rs` | 193 | `e213daa6eee2` |
| `tests/client/dot/mod.rs` | 4 | `b723021c3d7c` |
| `tests/client/dynamodb/CLAUDE.md` | 189 | `06fc308ef108` |
| `tests/client/dynamodb/command_channel_test.rs` | 404 | `e189ec7e6234` |
| `tests/client/dynamodb/e2e_test.rs` | 318 | `8ab5d90e14d9` |
| `tests/client/dynamodb/mod.rs` | 5 | `15c9f5d5ea45` |
| `tests/client/elasticsearch/CLAUDE.md` | 260 | `62679eca70f8` |
| `tests/client/elasticsearch/command_channel_test.rs` | 244 | `fa7978a8928e` |
| `tests/client/elasticsearch/e2e_test.rs` | 491 | `75bf68f87535` |
| `tests/client/elasticsearch/mod.rs` | 5 | `45b5f672e157` |
| `tests/client/etcd/CLAUDE.md` | 76 | `e729f899cf96` |
| `tests/client/etcd/command_channel_test.rs` | 233 | `69150cbd75a5` |
| `tests/client/etcd/e2e_test.rs` | 485 | `acf62eba3616` |
| `tests/client/etcd/mod.rs` | 6 | `e9820199ed24` |
| `tests/client/etcd/real_server_test.rs` | 178 | `f3e604190132` |
| `tests/client/finger/CLAUDE.md` | 80 | `838cbd31ee82` |
| `tests/client/finger/e2e_test.rs` | 498 | `26fa92fafd9b` |
| `tests/client/finger/mod.rs` | 2 | `85693194c197` |
| `tests/client/ftp/CLAUDE.md` | 59 | `6211ec956097` |
| `tests/client/ftp/command_channel_test.rs` | 172 | `40a8482d8e43` |
| `tests/client/ftp/mod.rs` | 4 | `ba16ddbde3d2` |
| `tests/client/ftp/test.rs` | 119 | `e5abff51de0f` |
| `tests/client/git/CLAUDE.md` | 316 | `2d29321d8b8b` |
| `tests/client/git/command_channel_test.rs` | 217 | `64cc6a44ae1e` |
| `tests/client/git/e2e_test.rs` | 162 | `73869c72f10b` |
| `tests/client/git/mod.rs` | 8 | `b2fcfad4b78f` |
| `tests/client/git/operation_events_test.rs` | 204 | `bf82685d4c4b` |
| `tests/client/git/sandbox_test.rs` | 812 | `758f836a6490` |
| `tests/client/gopher/CLAUDE.md` | 130 | `c328231652d1` |
| `tests/client/gopher/e2e_test.rs` | 576 | `ec0e015e44f4` |
| `tests/client/gopher/mod.rs` | 2 | `7e49fb3b226d` |
| `tests/client/grpc/CLAUDE.md` | 154 | `f2ecf639c933` |
| `tests/client/grpc/command_channel_test.rs` | 366 | `e54d93fc6601` |
| `tests/client/grpc/e2e_test.rs` | 223 | `32acf46adf9b` |
| `tests/client/grpc/mod.rs` | 6 | `5cacc9d37647` |
| `tests/client/helpers.rs` | 5 | `d0014d59c935` |
| `tests/client/http/CLAUDE.md` | 115 | `26d0e29db506` |
| `tests/client/http/command_channel_test.rs` | 199 | `13b8c647ee3a` |
| `tests/client/http/e2e_test.rs` | 251 | `6b9e97a038f8` |
| `tests/client/http/fetch_client_test.rs` | 293 | `72dd79da3f67` |
| `tests/client/http/mod.rs` | 10 | `a8e10ff742f2` |
| `tests/client/http/real_server_test.rs` | 183 | `043f05c93cd8` |
| `tests/client/http/transport_test.rs` | 312 | `e23e1f859ff8` |
| `tests/client/http2/CLAUDE.md` | 185 | `2fa09f870d2e` |
| `tests/client/http2/command_channel_test.rs` | 195 | `31b87f22342b` |
| `tests/client/http2/e2e_test.rs` | 372 | `d35658dd6c54` |
| `tests/client/http2/h2_transport_test.rs` | 134 | `2827eed6b2a6` |
| `tests/client/http2/mod.rs` | 6 | `93340df823ef` |
| `tests/client/http3/CLAUDE.md` | 306 | `94492c1ff461` |
| `tests/client/http3/command_channel_test.rs` | 144 | `6e1c9b0e3f0b` |
| `tests/client/http3/e2e_test.rs` | 358 | `1737d93db2a1` |
| `tests/client/http3/mod.rs` | 6 | `2c75e7a8b12a` |
| `tests/client/http_proxy/CLAUDE.md` | 200 | `86f8c8495ffb` |
| `tests/client/http_proxy/command_channel_test.rs` | 222 | `557b06c25fba` |
| `tests/client/http_proxy/e2e_test.rs` | 174 | `be4abab3362a` |
| `tests/client/http_proxy/mod.rs` | 7 | `c6c27c40f518` |
| `tests/client/http_proxy/target_port_range_test.rs` | 66 | `8d8c5459d084` |
| `tests/client/icmp/CLAUDE.md` | 150 | `6f39c1b1eba8` |
| `tests/client/icmp/action_codec_test.rs` | 249 | `02cd4e053726` |
| `tests/client/icmp/command_channel_test.rs` | 207 | `1e92edf1f749` |
| `tests/client/icmp/e2e_test.rs` | 104 | `9feeadf630fe` |
| `tests/client/icmp/mod.rs` | 6 | `e0ef2f80221f` |
| `tests/client/ident/CLAUDE.md` | 103 | `2e5ffaf8faca` |
| `tests/client/ident/e2e_test.rs` | 553 | `4bf1aae6c6f2` |
| `tests/client/ident/mod.rs` | 2 | `747988eea5c3` |
| `tests/client/igmp/CLAUDE.md` | 246 | `ec85b447a23b` |
| `tests/client/igmp/command_channel_test.rs` | 179 | `42641bed4743` |
| `tests/client/igmp/e2e_test.rs` | 262 | `398038c823f3` |
| `tests/client/igmp/mod.rs` | 4 | `f53df292ad45` |
| `tests/client/imap/CLAUDE.md` | 206 | `ee99b1297d83` |
| `tests/client/imap/command_channel_test.rs` | 223 | `d1eec6f7fe04` |
| `tests/client/imap/e2e_test.rs` | 655 | `8113ec6ab1b3` |
| `tests/client/imap/mod.rs` | 7 | `278f89fa5cc8` |
| `tests/client/imap/use_tls_refusal_test.rs` | 68 | `81846e76a879` |
| `tests/client/ipp/CLAUDE.md` | 244 | `57f472245271` |
| `tests/client/ipp/command_channel_test.rs` | 192 | `1a48291766ec` |
| `tests/client/ipp/document_encoding_test.rs` | 131 | `8d4122c87a88` |
| `tests/client/ipp/e2e_test.rs` | 346 | `f30f34d6d2b5` |
| `tests/client/ipp/mod.rs` | 8 | `5b8e5b894dad` |
| `tests/client/irc/CLAUDE.md` | 123 | `9d929686b5ec` |
| `tests/client/irc/command_channel_test.rs` | 179 | `3472232e9f02` |
| `tests/client/irc/e2e_test.rs` | 366 | `ea8aa9c6161a` |
| `tests/client/irc/framing_test.rs` | 267 | `2397ef8098c9` |
| `tests/client/irc/mod.rs` | 6 | `bbb54e6ceaf5` |
| `tests/client/isis/CLAUDE.md` | 394 | `3c31805f6117` |
| `tests/client/isis/capture_stop_test.rs` | 181 | `627b952ac4e7` |
| `tests/client/isis/command_channel_test.rs` | 184 | `fefb6b9be58d` |
| `tests/client/isis/e2e_test.rs` | 399 | `d9a57a4c444d` |
| `tests/client/isis/mod.rs` | 6 | `f8153d7af58a` |
| `tests/client/jsonrpc/CLAUDE.md` | 84 | `72ec20c978cb` |
| `tests/client/jsonrpc/command_channel_test.rs` | 197 | `be7ef1ed2dcd` |
| `tests/client/jsonrpc/e2e_test.rs` | 488 | `e8edd2b5ddfa` |
| `tests/client/jsonrpc/mod.rs` | 4 | `defe028c7a6c` |
| `tests/client/kafka/CLAUDE.md` | 111 | `25b3a99c8be8` |
| `tests/client/kafka/command_channel_test.rs` | 227 | `55e6f89109c4` |
| `tests/client/kafka/e2e_test.rs` | 419 | `1dcb41918308` |
| `tests/client/kafka/mod.rs` | 6 | `7c2f9d982f39` |
| `tests/client/kubernetes/CLAUDE.md` | 56 | `f0d99772816b` |
| `tests/client/kubernetes/command_channel_test.rs` | 313 | `b736d159646c` |
| `tests/client/kubernetes/e2e_test.rs` | 197 | `22f81a2a93d1` |
| `tests/client/kubernetes/mod.rs` | 5 | `46e90aa8d43d` |
| `tests/client/ldap/CLAUDE.md` | 90 | `60ee45e63d7a` |
| `tests/client/ldap/command_channel_test.rs` | 192 | `cec42b80474f` |
| `tests/client/ldap/mod.rs` | 4 | `502fcca3cee7` |
| `tests/client/ldap/real_server_test.rs` | 496 | `3a3196095733` |
| `tests/client/llmnr/CLAUDE.md` | 123 | `b2ff597edce6` |
| `tests/client/llmnr/e2e_test.rs` | 498 | `71e5b6329be3` |
| `tests/client/llmnr/mod.rs` | 2 | `eb655e710b56` |
| `tests/client/maven/CLAUDE.md` | 238 | `71fc7a50e6be` |
| `tests/client/maven/command_channel_test.rs` | 267 | `0b2b09bdb4a5` |
| `tests/client/maven/e2e_test.rs` | 197 | `695c5181d36a` |
| `tests/client/maven/mod.rs` | 5 | `c8bdb7e8558a` |
| `tests/client/mcp/CLAUDE.md` | 192 | `5925056d14fb` |
| `tests/client/mcp/command_channel_test.rs` | 265 | `0ada31ca523d` |
| `tests/client/mcp/e2e_test.rs` | 331 | `c1953935cbaf` |
| `tests/client/mcp/mod.rs` | 7 | `d8f9c5b5f3cc` |
| `tests/client/mdns/CLAUDE.md` | 207 | `0eff7133fc70` |
| `tests/client/mdns/command_channel_test.rs` | 161 | `219a75c94a82` |
| `tests/client/mdns/e2e_test.rs` | 249 | `c949b4fae43a` |
| `tests/client/mdns/mod.rs` | 4 | `4aa6bd752d46` |
| `tests/client/memcached/CLAUDE.md` | 50 | `f02877dc2986` |
| `tests/client/memcached/in_flight_test.rs` | 87 | `f92178a7cc04` |
| `tests/client/memcached/mod.rs` | 6 | `cdd6c650e4ff` |
| `tests/client/memcached/real_server_test.rs` | 329 | `c19789a2b000` |
| `tests/client/memcached/wire_test.rs` | 231 | `4e0d8b96a169` |
| `tests/client/mod.rs` | 208 | `4422bb699e85` |
| `tests/client/modbus/CLAUDE.md` | 41 | `ceadf84a29c7` |
| `tests/client/modbus/codec_test.rs` | 143 | `004bae971b5c` |
| `tests/client/modbus/mod.rs` | 6 | `c038a805b11e` |
| `tests/client/modbus/real_server_test.rs` | 343 | `83e6422a5e9e` |
| `tests/client/modbus/unanswered_test.rs` | 174 | `d0899d5fec0f` |
| `tests/client/mongodb/CLAUDE.md` | 470 | `be7b281f1d5d` |
| `tests/client/mongodb/command_channel_test.rs` | 193 | `d1b715c0f3a5` |
| `tests/client/mongodb/e2e_test.rs` | 369 | `92a43587b834` |
| `tests/client/mongodb/mod.rs` | 6 | `de366014576e` |
| `tests/client/mqtt/CLAUDE.md` | 107 | `f13aebac7b43` |
| `tests/client/mqtt/command_channel_test.rs` | 213 | `e02929fd2868` |
| `tests/client/mqtt/keepalive_test.rs` | 276 | `ef9da34b03cb` |
| `tests/client/mqtt/mod.rs` | 6 | `68e704fe12ef` |
| `tests/client/mqtt/real_server_test.rs` | 314 | `4076991430f9` |
| `tests/client/mssql/CLAUDE.md` | 147 | `e60a6c6550a1` |
| `tests/client/mssql/command_channel_test.rs` | 219 | `78f919649f17` |
| `tests/client/mssql/e2e_test.rs` | 268 | `d5a059658f00` |
| `tests/client/mssql/mod.rs` | 5 | `df00e2d7843f` |
| `tests/client/mysql/CLAUDE.md` | 92 | `d100c0d06bb0` |
| `tests/client/mysql/command_channel_test.rs` | 187 | `7628ec5819a6` |
| `tests/client/mysql/e2e_test.rs` | 432 | `9c6c1048b673` |
| `tests/client/mysql/mod.rs` | 6 | `9760d281120e` |
| `tests/client/mysql/real_server_test.rs` | 223 | `2c558585b1c6` |
| `tests/client/nats/CLAUDE.md` | 131 | `5faee177b667` |
| `tests/client/nats/e2e_test.rs` | 835 | `62bf8b28272d` |
| `tests/client/nats/mod.rs` | 5 | `f900550509cd` |
| `tests/client/netbios_ns/CLAUDE.md` | 144 | `28d7cdd85dbc` |
| `tests/client/netbios_ns/e2e_test.rs` | 720 | `b151082fe639` |
| `tests/client/netbios_ns/mod.rs` | 2 | `69d4d51536a6` |
| `tests/client/nfc/CLAUDE.md` | 93 | `043288a5ac82` |
| `tests/client/nfc/command_channel_test.rs` | 199 | `ff7a8e52dc66` |
| `tests/client/nfc/e2e_test.rs` | 460 | `5416685346cb` |
| `tests/client/nfc/mod.rs` | 4 | `cf87671ccd07` |
| `tests/client/nfs/CLAUDE.md` | 258 | `d2f0f20d5685` |
| `tests/client/nfs/command_channel_test.rs` | 200 | `bcdbab62e924` |
| `tests/client/nfs/e2e_test.rs` | 207 | `6dc3f6aa59b8` |
| `tests/client/nfs/mod.rs` | 4 | `e51c3c1e56a9` |
| `tests/client/nntp/CLAUDE.md` | 196 | `1e90d16c558b` |
| `tests/client/nntp/command_channel_test.rs` | 173 | `44ad748f439b` |
| `tests/client/nntp/e2e_test.rs` | 378 | `ba1f2ca6ab1a` |
| `tests/client/nntp/mod.rs` | 4 | `ad31b48ff1b3` |
| `tests/client/npm/CLAUDE.md` | 272 | `1faf8891de7f` |
| `tests/client/npm/command_channel_test.rs` | 357 | `506c90e8b531` |
| `tests/client/npm/e2e_test.rs` | 273 | `cebb2ee2f001` |
| `tests/client/npm/mod.rs` | 8 | `4f4a948754f4` |
| `tests/client/npm/registry_target_test.rs` | 143 | `6170d29d54b9` |
| `tests/client/ntp/CLAUDE.md` | 215 | `003a7ba9a02e` |
| `tests/client/ntp/command_channel_test.rs` | 166 | `0ad23d2b66af` |
| `tests/client/ntp/e2e_test.rs` | 183 | `c1e5ba10e3a5` |
| `tests/client/ntp/mod.rs` | 9 | `70512d2dc25f` |
| `tests/client/oauth2/CLAUDE.md` | 318 | `fd22bb39cac8` |
| `tests/client/oauth2/command_channel_test.rs` | 258 | `b88a8e546350` |
| `tests/client/oauth2/e2e_test.rs` | 421 | `5ceb963ebe8e` |
| `tests/client/oauth2/mod.rs` | 4 | `46f082653e5a` |
| `tests/client/ollama/CLAUDE.md` | 336 | `e15a17bbb52f` |
| `tests/client/ollama/command_channel_test.rs` | 254 | `590d776a2b46` |
| `tests/client/ollama/e2e_test.rs` | 532 | `3485a4a4752b` |
| `tests/client/ollama/endpoint_targeting_test.rs` | 246 | `133749e9cdfb` |
| `tests/client/ollama/mod.rs` | 6 | `4a2267817cdf` |
| `tests/client/openai/CLAUDE.md` | 254 | `c6ad01d6d36c` |
| `tests/client/openai/command_channel_test.rs` | 285 | `56f837af75db` |
| `tests/client/openai/e2e_test.rs` | 267 | `d10dc3ee4704` |
| `tests/client/openai/endpoint_and_limits_test.rs` | 233 | `b3d4a4b8446e` |
| `tests/client/openai/mod.rs` | 6 | `7542a010653b` |
| `tests/client/openapi/CLAUDE.md` | 227 | `e29e889b807a` |
| `tests/client/openapi/command_channel_test.rs` | 241 | `c32ca35302a7` |
| `tests/client/openapi/e2e_test.rs` | 223 | `393bfb72fbdb` |
| `tests/client/openapi/mod.rs` | 8 | `742299851579` |
| `tests/client/openapi/target_precedence_test.rs` | 245 | `26b2c9b8bdec` |
| `tests/client/openapi/test-api.yaml` | 116 | `a49a0792aa6d` |
| `tests/client/openidconnect/CLAUDE.md` | 67 | `22553f513045` |
| `tests/client/openidconnect/command_channel_test.rs` | 256 | `d0906f9ea330` |
| `tests/client/openidconnect/e2e_test.rs` | 118 | `6e801ba3cc0d` |
| `tests/client/openidconnect/mod.rs` | 4 | `b03bd5177fed` |
| `tests/client/ospf/CLAUDE.md` | 392 | `ca592aa7efee` |
| `tests/client/ospf/command_channel_test.rs` | 216 | `f382cc886e09` |
| `tests/client/ospf/e2e_test.rs` | 180 | `e381b0bd9a13` |
| `tests/client/ospf/mod.rs` | 4 | `4cbfe57fa1ea` |
| `tests/client/pop3/CLAUDE.md` | 216 | `2109539a92d0` |
| `tests/client/pop3/command_channel_test.rs` | 150 | `5980595e9fb9` |
| `tests/client/pop3/e2e_test.rs` | 207 | `eb89c5d993cf` |
| `tests/client/pop3/mod.rs` | 6 | `ccaed71bbcec` |
| `tests/client/pop3/use_tls_refusal_test.rs` | 65 | `a7ab16f4bf3e` |
| `tests/client/postgresql/CLAUDE.md` | 78 | `1c40f0f64447` |
| `tests/client/postgresql/command_channel_test.rs` | 187 | `04efb6651123` |
| `tests/client/postgresql/e2e_test.rs` | 110 | `f1c345b33544` |
| `tests/client/postgresql/mod.rs` | 6 | `247bdc547c40` |
| `tests/client/postgresql/real_server_test.rs` | 221 | `45527e4b7671` |
| `tests/client/pypi/CLAUDE.md` | 285 | `d4a7194148c9` |
| `tests/client/pypi/command_channel_test.rs` | 220 | `249984282d60` |
| `tests/client/pypi/e2e_test.rs` | 170 | `563909a3a456` |
| `tests/client/pypi/index_target_test.rs` | 130 | `70d90b4d0766` |
| `tests/client/pypi/mod.rs` | 10 | `41c2ed9239d0` |
| `tests/client/radius/CLAUDE.md` | 41 | `aa94db86116c` |
| `tests/client/radius/mod.rs` | 6 | `de1529aa61a3` |
| `tests/client/radius/real_server_test.rs` | 453 | `463ac0149508` |
| `tests/client/radius/request_test.rs` | 137 | `c9c25274573c` |
| `tests/client/radius/transport_test.rs` | 343 | `2386670484da` |
| `tests/client/redis/CLAUDE.md` | 91 | `7e97e1f6071c` |
| `tests/client/redis/command_channel_test.rs` | 166 | `2793ac10c645` |
| `tests/client/redis/e2e_test.rs` | 217 | `cd2333eef9d5` |
| `tests/client/redis/mod.rs` | 8 | `f495b870c9bb` |
| `tests/client/redis/real_server_test.rs` | 261 | `0addd82a1059` |
| `tests/client/redis/resp_reader_test.rs` | 192 | `91ca38147a11` |
| `tests/client/rip/CLAUDE.md` | 77 | `295c37af788c` |
| `tests/client/rip/command_channel_test.rs` | 179 | `29cf362a7a5b` |
| `tests/client/rip/e2e_test.rs` | 60 | `ca09c6ce7b55` |
| `tests/client/rip/llm_path_test.rs` | 191 | `ed769acc8dde` |
| `tests/client/rip/mod.rs` | 6 | `561ff5b96481` |
| `tests/client/rss/CLAUDE.md` | 84 | `cb4a01672969` |
| `tests/client/rss/command_channel_test.rs` | 197 | `4c392d6a55fe` |
| `tests/client/rss/e2e_test.rs` | 194 | `42f0b6e4cd4f` |
| `tests/client/rss/mod.rs` | 4 | `89b15d625893` |
| `tests/client/s3/CLAUDE.md` | 358 | `f6153ecc8c69` |
| `tests/client/s3/command_channel_test.rs` | 323 | `1ecaf2e54dcd` |
| `tests/client/s3/e2e_test.rs` | 166 | `542ac9f6424c` |
| `tests/client/s3/mod.rs` | 6 | `76dbbafdca46` |
| `tests/client/saml/CLAUDE.md` | 169 | `f02601297f6a` |
| `tests/client/saml/command_channel_test.rs` | 212 | `8a72e6e9a4b8` |
| `tests/client/saml/e2e_test.rs` | 111 | `bac048694b71` |
| `tests/client/saml/mod.rs` | 8 | `68a401457836` |
| `tests/client/saml/startup_params_test.rs` | 234 | `9a540ace0c23` |
| `tests/client/saml/status_code_test.rs` | 173 | `ae412c8af064` |
| `tests/client/sip/CLAUDE.md` | 356 | `3514c3db5af8` |
| `tests/client/sip/command_channel_test.rs` | 192 | `2469ca22ae2c` |
| `tests/client/sip/e2e_test.rs` | 375 | `fb5b5f626b60` |
| `tests/client/sip/hostile_response_test.rs` | 196 | `f26040e607ea` |
| `tests/client/sip/mod.rs` | 6 | `f8c4d716fe46` |
| `tests/client/smb/CLAUDE.md` | 407 | `deb4f5e0ce50` |
| `tests/client/smb/command_channel_test.rs` | 171 | `2deb97b58bbe` |
| `tests/client/smb/e2e_test.rs` | 147 | `d1b24e3eaefa` |
| `tests/client/smb/mod.rs` | 4 | `b5c0afb68b4a` |
| `tests/client/smtp/CLAUDE.md` | 186 | `dbb1bc4e0e58` |
| `tests/client/smtp/command_channel_test.rs` | 244 | `0cf8afe26e40` |
| `tests/client/smtp/e2e_test.rs` | 171 | `32900ca64ae1` |
| `tests/client/smtp/mod.rs` | 6 | `f40801429575` |
| `tests/client/smtp/startup_params_test.rs` | 294 | `bea596550340` |
| `tests/client/snmp/CLAUDE.md` | 272 | `fce325c92d15` |
| `tests/client/snmp/command_channel_test.rs` | 186 | `189775d37236` |
| `tests/client/snmp/e2e_test.rs` | 315 | `944c71d41d13` |
| `tests/client/snmp/mod.rs` | 4 | `c3560bd2cdc4` |
| `tests/client/socket_file/CLAUDE.md` | 102 | `63fc2298fbd1` |
| `tests/client/socket_file/command_channel_test.rs` | 146 | `a4dc88fed709` |
| `tests/client/socket_file/e2e_test.rs` | 264 | `c493ff6fa83f` |
| `tests/client/socket_file/mod.rs` | 6 | `a43210ff80c1` |
| `tests/client/socks5/CLAUDE.md` | 381 | `c4ecddbfabdf` |
| `tests/client/socks5/action_test.rs` | 391 | `aa2cd1849403` |
| `tests/client/socks5/command_channel_test.rs` | 193 | `870bda2b1f6e` |
| `tests/client/socks5/e2e_test.rs` | 312 | `751e5c6077ae` |
| `tests/client/socks5/mod.rs` | 7 | `96cbaaaf517c` |
| `tests/client/sqs/CLAUDE.md` | 219 | `0af822b0d10e` |
| `tests/client/sqs/command_channel_test.rs` | 256 | `c5217db1a6a2` |
| `tests/client/sqs/e2e_test.rs` | 231 | `57c0bad044a4` |
| `tests/client/sqs/mod.rs` | 7 | `d56dda6fae99` |
| `tests/client/ssdp/CLAUDE.md` | 137 | `a249f79cb7c4` |
| `tests/client/ssdp/e2e_test.rs` | 608 | `022cba3fd0fd` |
| `tests/client/ssdp/mod.rs` | 2 | `08840d5e2ded` |
| `tests/client/ssh/CLAUDE.md` | 88 | `ab2d6e22b489` |
| `tests/client/ssh/command_channel_test.rs` | 241 | `dee13f87344f` |
| `tests/client/ssh/mod.rs` | 4 | `475716b73eee` |
| `tests/client/ssh/real_server_test.rs` | 327 | `16db20540a21` |
| `tests/client/ssh_agent/CLAUDE.md` | 211 | `04162a454703` |
| `tests/client/ssh_agent/command_channel_test.rs` | 150 | `a3de9b2fc4f6` |
| `tests/client/ssh_agent/e2e_test.rs` | 249 | `49bf7d6dd743` |
| `tests/client/ssh_agent/mod.rs` | 4 | `213c2156432f` |
| `tests/client/stomp/CLAUDE.md` | 98 | `8bcbb722ca4a` |
| `tests/client/stomp/e2e_test.rs` | 573 | `6105da91d59e` |
| `tests/client/stomp/mod.rs` | 2 | `801fc68b8cc8` |
| `tests/client/stun/CLAUDE.md` | 62 | `df6525590d16` |
| `tests/client/stun/command_channel_test.rs` | 198 | `94393b7ea82c` |
| `tests/client/stun/e2e_test.rs` | 122 | `e3a943062233` |
| `tests/client/stun/mod.rs` | 5 | `a5ec36555f3e` |
| `tests/client/syslog/CLAUDE.md` | 79 | `5581b837d8c3` |
| `tests/client/syslog/command_channel_test.rs` | 252 | `8242ef99cd38` |
| `tests/client/syslog/e2e_test.rs` | 163 | `271d5e2914c5` |
| `tests/client/syslog/mod.rs` | 4 | `1fa2b674ec12` |
| `tests/client/tcp/CLAUDE.md` | 40 | `da7ca3957370` |
| `tests/client/tcp/e2e_test.rs` | 229 | `d08fd22036aa` |
| `tests/client/tcp/mod.rs` | 2 | `26256c99ab8c` |
| `tests/client/telnet/CLAUDE.md` | 170 | `5739449bc74a` |
| `tests/client/telnet/e2e_test.rs` | 305 | `2fa493d81366` |
| `tests/client/telnet/mod.rs` | 2 | `ea32e7e6cc08` |
| `tests/client/tftp/CLAUDE.md` | 75 | `0127623deb11` |
| `tests/client/tftp/command_channel_test.rs` | 178 | `9b07196c3fa3` |
| `tests/client/tftp/e2e_test.rs` | 255 | `74957cdaf923` |
| `tests/client/tftp/mod.rs` | 5 | `ebb20639224c` |
| `tests/client/tls/CLAUDE.md` | 173 | `646882929207` |
| `tests/client/tls/command_channel_test.rs` | 179 | `c050054ac824` |
| `tests/client/tls/e2e_test.rs` | 410 | `6bb2779d9f30` |
| `tests/client/tls/mod.rs` | 8 | `531f938f97fd` |
| `tests/client/tls/multi_turn_test.rs` | 147 | `e828d24b5ef5` |
| `tests/client/tor/CLAUDE.md` | 109 | `02376d38be1f` |
| `tests/client/tor/action_test.rs` | 391 | `dfce3c6810a0` |
| `tests/client/tor/apply_actions_test.rs` | 126 | `b29b3f587a6d` |
| `tests/client/tor/command_channel_test.rs` | 80 | `03d0bb353954` |
| `tests/client/tor/e2e_test.rs` | 159 | `0d8311e97114` |
| `tests/client/tor/mod.rs` | 16 | `8f5fe90d2114` |
| `tests/client/tor/test.rs` | 164 | `7ae94b50e839` |
| `tests/client/torrent_dht/CLAUDE.md` | 51 | `4c87abbe6007` |
| `tests/client/torrent_dht/command_channel_test.rs` | 231 | `22c306111240` |
| `tests/client/torrent_dht/e2e_test.rs` | 97 | `6932515b2e58` |
| `tests/client/torrent_dht/mod.rs` | 4 | `59a3e548831f` |
| `tests/client/torrent_peer/CLAUDE.md` | 53 | `6599f0b07fce` |
| `tests/client/torrent_peer/command_channel_test.rs` | 186 | `4f7359366311` |
| `tests/client/torrent_peer/e2e_test.rs` | 116 | `e8ebf6571623` |
| `tests/client/torrent_peer/mod.rs` | 5 | `d1b357d09c0b` |
| `tests/client/torrent_tracker/command_channel_test.rs` | 451 | `6f0aa5e17a89` |
| `tests/client/torrent_tracker/followup_chain_test.rs` | 178 | `b0fdbc8b5612` |
| `tests/client/torrent_tracker/mod.rs` | 5 | `d010262dcdd8` |
| `tests/client/turn/CLAUDE.md` | 228 | `899374b26980` |
| `tests/client/turn/command_channel_test.rs` | 236 | `fc47f63420ce` |
| `tests/client/turn/e2e_test.rs` | 299 | `d36c654d9b6b` |
| `tests/client/turn/mod.rs` | 6 | `8bf42bcfd978` |
| `tests/client/turn/response_parsing_test.rs` | 235 | `a184448de829` |
| `tests/client/udp/CLAUDE.md` | 210 | `5a940c16ad1d` |
| `tests/client/udp/command_channel_test.rs` | 177 | `c1d90f342e5b` |
| `tests/client/udp/e2e_test.rs` | 403 | `a6906a44c61a` |
| `tests/client/udp/mod.rs` | 5 | `0ae49bafa209` |
| `tests/client/usb/CLAUDE.md` | 204 | `c8c9106e2be7` |
| `tests/client/usb/command_channel_test.rs` | 168 | `2c6b8333c59b` |
| `tests/client/usb/e2e_test.rs` | 252 | `cd404c9f9168` |
| `tests/client/usb/mod.rs` | 4 | `cee565ae799c` |
| `tests/client/vnc/CLAUDE.md` | 205 | `22d097ceda3a` |
| `tests/client/vnc/command_channel_test.rs` | 164 | `563c7ad45d22` |
| `tests/client/vnc/coordinate_range_test.rs` | 114 | `3b2564b6804b` |
| `tests/client/vnc/e2e_test.rs` | 203 | `61fcd8e2c372` |
| `tests/client/vnc/mod.rs` | 6 | `bb2325a61544` |
| `tests/client/webdav/CLAUDE.md` | 109 | `6f7cbedf449a` |
| `tests/client/webdav/command_channel_test.rs` | 249 | `5110195cebe8` |
| `tests/client/webdav/e2e_test.rs` | 250 | `9aa86f2e2fae` |
| `tests/client/webdav/mod.rs` | 5 | `ecadb7bf5902` |
| `tests/client/webrtc/CLAUDE.md` | 336 | `4fdd18e0db8e` |
| `tests/client/webrtc/command_channel_test.rs` | 176 | `c6c36367401d` |
| `tests/client/webrtc/e2e_test.rs` | 151 | `b07d44d7afbd` |
| `tests/client/webrtc/mod.rs` | 4 | `7b7ea5c5cd87` |
| `tests/client/websocket/CLAUDE.md` | 97 | `e380932cfbf9` |
| `tests/client/websocket/command_channel_test.rs` | 198 | `d65c5cc19bbb` |
| `tests/client/websocket/e2e_test.rs` | 364 | `c1a16421078c` |
| `tests/client/websocket/keepalive_test.rs` | 239 | `605ac03edf06` |
| `tests/client/websocket/mod.rs` | 5 | `9a0b45e2cd92` |
| `tests/client/whois/CLAUDE.md` | 79 | `895c85baea15` |
| `tests/client/whois/command_channel_test.rs` | 169 | `9f1f92d0fb26` |
| `tests/client/whois/e2e_test.rs` | 159 | `6e294063698e` |
| `tests/client/whois/mod.rs` | 4 | `a9db3043d278` |
| `tests/client/wireguard/CLAUDE.md` | 206 | `11fe8ec8de12` |
| `tests/client/wireguard/command_channel_test.rs` | 178 | `b67e0355f066` |
| `tests/client/wireguard/e2e_test.rs` | 321 | `5de773baa296` |
| `tests/client/wireguard/mod.rs` | 4 | `3f758bb57455` |
| `tests/client/xmlrpc/command_channel_test.rs` | 199 | `4acd5a627edb` |
| `tests/client/xmlrpc/mod.rs` | 4 | `19da255faa27` |
| `tests/client/xmlrpc/response_guard_test.rs` | 223 | `c21a891a79d6` |
| `tests/client/xmpp/CLAUDE.md` | 280 | `dc32b2e346f7` |
| `tests/client/xmpp/command_channel_test.rs` | 152 | `971f2e71a2df` |
| `tests/client/xmpp/e2e_test.rs` | 190 | `76adab975c5a` |
| `tests/client/xmpp/mod.rs` | 6 | `cb9b8625b734` |
| `tests/client/xmpp/startup_params_test.rs` | 126 | `25e49522f8e7` |
| `tests/client/zookeeper/command_channel_test.rs` | 279 | `08221adec4ce` |
| `tests/client/zookeeper/mod.rs` | 2 | `261e3400e5c0` |
