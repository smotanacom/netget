# Test infrastructure, examples, prompts, schema and vendor review — 2026-10-01

## Scope and constraints

167 files inventoried, including helper libraries, evaluation framework, live-model suite definitions, examples, prompt templates, etcd protobufs and vendored hyper. Every file participated in the text/inventory sweep; focused manual review covered helper mode selection, deadlines, process management, examples, eval probe lifecycle and the local vendor patch. This is a static audit, not a claim of full execution or equal manual depth across upstream vendor code.

No GPU/model workloads, model availability probes, live protocol suites, package installation or external peer sessions were run. Root owns Cargo verification. The new regression target is explicitly CPU-only.

## Changes implemented

| File(s) | Defect | Improvement | Evidence |
|---|---|---|---|
| `tests/helpers/common.rs`, `netget.rs`, `llm_live.rs` | Presence of `NETGET_USE_OLLAMA` selected real inference, including empty string,0,false. | Shared opt-in parser accepts1,true,yes,on (case-insensitive, trimmed); everything else stays off. | Pure regression covers unset,empty,negative choices,typo and affirmative values; no environment mutation. |
| `tests/helpers/common.rs` | Binary resolver checked Cargo executable location only at runtime; Cargo typically supplies it at compile time, allowing fallback to an unrelated newer debug/release binary. | Use `option_env!("CARGO_BIN_EXE_netget")` before manual fallback; keep explicit runtime override. Missing Cargo-built binary produces a clear error. | Regression compares selected executable to Cargo compile-time path. |
| `tests/helpers/common.rs` | Retry total timeout did not bound a single pending condition or a long backoff sleep. Duration multiplication could overflow. | Bound each attempt by remaining total time, clamp sleep to remaining time and double delay with checked arithmetic. | Pending future,20-second requested sleep against30ms budget, and transient-failure recovery tests. |
| `examples/external_protocol/src/lib.rs` | Example implemented common methods on Server although they belong to Protocol; used legacy metadata and omitted required fields/methods. Spawned untracked listener/peer tasks. | Implement current Protocol and Server traits, ProtocolMetadataV2, startup examples,group and log_template; register all server tasks with AppState; label Experimental; return on listener failure. | Root regression includes this exact source,checks action encoding/startup shape,and performs loopback echo followed by server removal and peer EOF. |
| `examples/external_protocol/Cargo.toml` | Example inherited the entire default protocol dependency tree. | Disable NetGet default features for its public-API demonstration. | Standalone manifest check delegated to root; root source inclusion checks API compatibility. |
| `examples/external_protocol/README.md` | Advertised nonexistent script paths,obsolete traits/maturity states,dynamic loading,and a dependency cycle. | Document actual embedding API,current root commands,deterministic behavior,registration requirements and limitations. | Cross-read with source/current traits. |
| `examples/test_doc_gen.rs` | Doc/template errors printed but process still exited successfully. | Return anyhow::Result and propagate failures through main. | Compile-check delegated to root; no model call required. |

## New regression target

`tests/test_infrastructure_review_test.rs`:7 CPU-only tests. Five cover shared helper selection/deadlines; two compile the actual external example and check its action/lifecycle behavior. The loopback echo test constructs an OllamaClient only because SpawnContext requires one; neither echo implementation nor test calls it.

Suggested centralized verification (force live-model variables absent):

```sh
env -u NETGET_USE_OLLAMA -u NETGET_LLM_TEST_MODEL -u OLLAMA_MODEL \
  ./cargo-isolated.sh test --offline --no-default-features --features tcp,http \
  --test test_infrastructure_review_test -- --test-threads=100
./cargo-isolated.sh check --offline --manifest-path examples/external_protocol/Cargo.toml
```

No result is asserted here until the root agent finishes those commands. Formatting and `git diff --check` were run locally.

## Static validation and useful findings

- All43 `.rs` suite files other than `mod.rs` in `tests/llm_live` are declared in its module tree; no orphan suite file found. Live entry points use the shared gating framework. No live case was executed.
- Compared every retained vendored hyper file to the locally cached crates.io hyper1.7.0 source. Differences are **only** `Cargo.toml`, `README.md`, `src/common/date.rs`, matching the documented wasm Date.now patch. Native code is preserved. No upstream vendor code was changed.
- Read the etcd KV service/message schema surface and field numbering,including recursive transaction request/response oneofs and Compare range_end tag64; no schema edits proposed without service wire interoperability evidence.
- Read prompt template inventory and network-request composition. The network-request path intentionally omits setup-only scripting instructions; no prompt-quality change was made based on static intuition without evaluation evidence.
- The sample external crate is intentionally not runtime loaded. It is a deterministic echo tutorial, not a complete LLM event protocol; advertised encoder behavior and listener behavior are described separately in its README.

## Remaining findings / boundaries

1. Eval probe writes all stdin before starting its output-read loop and before enforcing the main deadline. A child that never reads stdin,or fills stdout while waiting for input,can stall the harness. A complete fix should drive input/output concurrently and preserve hold_stdin semantics,with subprocess regressions; no live model needed.
2. Eval probe output buffers and its final drain are time-limited but not byte-limited. A chatty child can allocate heavily before a deadline. Choosing a retention cap needs explicit truncated-output reporting so scoring never mistakes truncated output for a model error.
3. Eval probe binary_available tests is_file rather than executability,and PATH parsing uses Unix colon splitting. Cross-platform process discovery can give misleading availability diagnoses.
4. Live real-model tests intentionally skip without affirmative opt-in and are evaluation tools rather than protocol-regression evidence. The gate fix reduces accidental opt-in; it does not imply all tests in the repository are CPU-safe if .with_ollama() is explicitly invoked.
5. get_available_port/replace_port_placeholders still bind then drop reservations,creating a race; fixing centrally requires keeping guards alive through process startup or using actual port0 bound-address discovery.
6. Helpers have historical docs and timing heuristics that remain broader than this targeted pass. Existing test readiness/mocked expectations must be revalidated before broad timing changes.
7. The vendor comparison establishes local patch scope,not upstream security currency or absence of vulnerabilities. No dependency upgrade was attempted.
8. Proto schemas were inspected statically; no live etcd peer or protoc compatibility job was run by this reviewer.
9. Prompt and live-suite coverage is a definitions/documentation review; no claim about model quality or reproducibility can be inferred without live evaluation,which the user expressly excluded.

## Coverage ledger

| Area | Files | Lines | Review depth |
|---|---:|---:|---|
| `tests/helpers` | 23 | 13537 | Focused: mode selection,binary resolution,retries,child ownership;remaining files static sweep |
| `tests/eval` | 9 | 5880 | Focused static: probe process I/O,classification/reporting contracts;no live runs |
| `tests/llm_live` | 44 | 14130 | Suite/module inventory,gate and framework review;no live runs |
| `examples` | 6 | 10107 | Focused source/API/documentation modernization |
| `prompts` | 18 | 1050 | Template composition and placeholder inventory;no model-quality claims |
| `proto` | 2 | 301 | Static service/message/tag review |
| `vendor` | 65 | 21553 | All-file local upstream comparison plus focused wasm date patch review |

## File inventory

| File | Lines | SHA-256 prefix |
|---|---:|---|
| `examples/external_protocol/.gitignore` | 1 | `34a04005bcaf` |
| `examples/external_protocol/Cargo.lock` | 9836 | `e30a362c24ad` |
| `examples/external_protocol/Cargo.toml` | 18 | `d96f012d01b8` |
| `examples/external_protocol/README.md` | 41 | `c50454cfd886` |
| `examples/external_protocol/src/lib.rs` | 151 | `831606aa5e48` |
| `examples/test_doc_gen.rs` | 60 | `3a0ce3adaf1b` |
| `prompts/easy_request/http.hbs` | 64 | `32db1be51985` |
| `prompts/easy_request/main.hbs` | 34 | `e029dd50ac7e` |
| `prompts/feedback/main.hbs` | 11 | `57c0fb1ef1f4` |
| `prompts/feedback/partials/instructions.hbs` | 42 | `b632a63ced5a` |
| `prompts/feedback/task.hbs` | 4 | `4bb4039325d5` |
| `prompts/network_request/main.hbs` | 46 | `fa8db5c2e467` |
| `prompts/network_request/partials/instructions.hbs` | 29 | `6ecf38ea777a` |
| `prompts/network_request/task.hbs` | 5 | `fdeea8fc5b6d` |
| `prompts/shared/partials/actions.hbs` | 62 | `a6f80597513b` |
| `prompts/shared/partials/base_stack_docs.hbs` | 4 | `76fbf0fad404` |
| `prompts/shared/partials/current_state.hbs` | 36 | `03f333d7d8cc` |
| `prompts/shared/partials/memory.hbs` | 12 | `7d6cd52a9f59` |
| `prompts/shared/partials/response_format.hbs` | 133 | `71702088e85c` |
| `prompts/shared/partials/role.hbs` | 3 | `720c396dce46` |
| `prompts/shared/partials/scripting.hbs` | 425 | `5cf462abe95c` |
| `prompts/user_input/main.hbs` | 27 | `e4408209edc9` |
| `prompts/user_input/partials/instructions.hbs` | 82 | `e1fee7914862` |
| `prompts/user_input/task.hbs` | 31 | `3da902cb5d3d` |
| `proto/etcd/kv.proto` | 41 | `fb2ff44e5f4b` |
| `proto/etcd/rpc.proto` | 260 | `c5918aff3cd2` |
| `tests/eval/CLAUDE.md` | 294 | `02d3b6223e37` |
| `tests/eval/case.rs` | 306 | `e7b28d9900da` |
| `tests/eval/classify.rs` | 682 | `5d4d18d3e152` |
| `tests/eval/mod.rs` | 53 | `5bd2d3a43eaa` |
| `tests/eval/probe.rs` | 270 | `7365c9411f21` |
| `tests/eval/probe_check.rs` | 1085 | `caee8982007c` |
| `tests/eval/report.rs` | 579 | `0ab222495ff9` |
| `tests/eval/runner.rs` | 556 | `be4314d9f82d` |
| `tests/eval/suites.rs` | 2055 | `7e5f6d3b2c73` |
| `tests/helpers/child_guard.rs` | 384 | `f1899c8c99bd` |
| `tests/helpers/client.rs` | 524 | `46e9417802f4` |
| `tests/helpers/common.rs` | 527 | `2d16c238fe6b` |
| `tests/helpers/event_trigger.rs` | 339 | `efcc9ec23d5c` |
| `tests/helpers/example_test_framework.rs` | 412 | `e067159c4777` |
| `tests/helpers/http_bounds.rs` | 665 | `147e2b061557` |
| `tests/helpers/inbound_limit.rs` | 281 | `78a7568496d8` |
| `tests/helpers/llm_live.rs` | 691 | `16a2642915aa` |
| `tests/helpers/llm_live_case.rs` | 465 | `236311b4d214` |
| `tests/helpers/mock.rs` | 3 | `2c118103f78d` |
| `tests/helpers/mock_action_names.rs` | 148 | `d43a408e95ca` |
| `tests/helpers/mock_builder.rs` | 380 | `d4f5fc6e771d` |
| `tests/helpers/mock_config.rs` | 762 | `f3cc62297d92` |
| `tests/helpers/mock_matcher.rs` | 402 | `19175ac399bb` |
| `tests/helpers/mock_ollama.rs` | 1307 | `8e75aec6c0d5` |
| `tests/helpers/mod.rs` | 40 | `1c734c61e663` |
| `tests/helpers/netget.rs` | 1449 | `ee4a39303dec` |
| `tests/helpers/ollama_test_builder.rs` | 1208 | `eee9466080e1` |
| `tests/helpers/pcap_oracle.rs` | 1147 | `08b0d2be18b4` |
| `tests/helpers/real_server.rs` | 764 | `de894b2c692a` |
| `tests/helpers/server.rs` | 705 | `2d681bb18986` |
| `tests/helpers/usbip_bounds.rs` | 290 | `a3597d2afbed` |
| `tests/helpers/usbip_client.rs` | 644 | `da0a7bc11fc4` |
| `tests/llm_live/CLAUDE.md` | 138 | `7950faf9c920` |
| `tests/llm_live/bigdata.rs` | 631 | `e6f93fbaab2e` |
| `tests/llm_live/ble_profiles.rs` | 584 | `cfe39477d11c` |
| `tests/llm_live/bluetooth_ble.rs` | 268 | `9707e949c318` |
| `tests/llm_live/couchdb.rs` | 180 | `d550fbf33f50` |
| `tests/llm_live/datastores.rs` | 1156 | `e3bfa6048d89` |
| `tests/llm_live/dns.rs` | 92 | `9decd1559aa8` |
| `tests/llm_live/dns_secure.rs` | 208 | `7854a91511f2` |
| `tests/llm_live/elasticsearch.rs` | 144 | `9a779383276f` |
| `tests/llm_live/federation.rs` | 357 | `b376041ac22e` |
| `tests/llm_live/ftp.rs` | 49 | `2f573bad5dea` |
| `tests/llm_live/http.rs` | 164 | `dfb30a40f050` |
| `tests/llm_live/http_apis.rs` | 1843 | `bc17dac7bf4c` |
| `tests/llm_live/imap.rs` | 138 | `7879661ae1b3` |
| `tests/llm_live/irc.rs` | 131 | `1fd877990081` |
| `tests/llm_live/jsonrpc.rs` | 74 | `86f8a4573652` |
| `tests/llm_live/memcached.rs` | 53 | `22f6f6ccb4ea` |
| `tests/llm_live/mod.rs` | 94 | `f77f7a4bdba2` |
| `tests/llm_live/netservices.rs` | 921 | `07e6e335a463` |
| `tests/llm_live/nfc.rs` | 216 | `964d11892438` |
| `tests/llm_live/nntp.rs` | 144 | `462c762ac53b` |
| `tests/llm_live/ntp.rs` | 74 | `c0f054c28624` |
| `tests/llm_live/openai.rs` | 217 | `9b4a4a4fcbcd` |
| `tests/llm_live/p2p.rs` | 503 | `c2f3f49c8d25` |
| `tests/llm_live/pop3.rs` | 50 | `8ec89234adde` |
| `tests/llm_live/rawnet.rs` | 245 | `0d8c0ba93908` |
| `tests/llm_live/realtime.rs` | 627 | `ba139d3eb258` |
| `tests/llm_live/redis.rs` | 109 | `3fdda1656a94` |
| `tests/llm_live/remote_access.rs` | 475 | `18a11b1219e7` |
| `tests/llm_live/routing.rs` | 558 | `594879721fe1` |
| `tests/llm_live/rss.rs` | 50 | `87e1c329cfb2` |
| `tests/llm_live/rtsp.rs` | 409 | `239005917110` |
| `tests/llm_live/sip.rs` | 370 | `03358366cfac` |
| `tests/llm_live/smtp.rs` | 56 | `b6021af5950d` |
| `tests/llm_live/socks5.rs` | 184 | `23cac975e30f` |
| `tests/llm_live/streams.rs` | 1153 | `0f82c652d81d` |
| `tests/llm_live/stun.rs` | 84 | `44f803aa1a28` |
| `tests/llm_live/tcp.rs` | 136 | `2fd3b2ed4feb` |
| `tests/llm_live/telnet.rs` | 140 | `dbd6e5009aad` |
| `tests/llm_live/udp.rs` | 69 | `7dbf03071605` |
| `tests/llm_live/usb.rs` | 636 | `a0fe0af8b49a` |
| `tests/llm_live/vpn.rs` | 207 | `3753724a1ce2` |
| `tests/llm_live/whois.rs` | 51 | `4bea51de5948` |
| `tests/llm_live/xmlrpc.rs` | 142 | `a061ffbf6d40` |
| `vendor/hyper/Cargo.toml` | 263 | `893c7bae9528` |
| `vendor/hyper/LICENSE` | 19 | `8f2eee700f56` |
| `vendor/hyper/README.md` | 118 | `60ec18cc4119` |
| `vendor/hyper/src/body/incoming.rs` | 628 | `c472cf4f29f9` |
| `vendor/hyper/src/body/length.rs` | 129 | `fcf245cd9d46` |
| `vendor/hyper/src/body/mod.rs` | 50 | `6e029d258d08` |
| `vendor/hyper/src/cfg.rs` | 44 | `de5fee5bba45` |
| `vendor/hyper/src/client/conn/http1.rs` | 611 | `7f5b1ebf50dd` |
| `vendor/hyper/src/client/conn/http2.rs` | 718 | `fac786e1348c` |
| `vendor/hyper/src/client/conn/mod.rs` | 22 | `9a3a11f287ac` |
| `vendor/hyper/src/client/dispatch.rs` | 523 | `2b660505c780` |
| `vendor/hyper/src/client/mod.rs` | 22 | `3428a487d81d` |
| `vendor/hyper/src/client/tests.rs` | 261 | `de0001609ef0` |
| `vendor/hyper/src/common/buf.rs` | 150 | `6ffe7941d14e` |
| `vendor/hyper/src/common/date.rs` | 157 | `947b43820a22` |
| `vendor/hyper/src/common/either.rs` | 46 | `cf162a408741` |
| `vendor/hyper/src/common/future.rs` | 30 | `dbeb3a936470` |
| `vendor/hyper/src/common/io/compat.rs` | 150 | `e3e9333b8c18` |
| `vendor/hyper/src/common/io/mod.rs` | 7 | `1f9002411f8a` |
| `vendor/hyper/src/common/io/rewind.rs` | 162 | `2a3f3d7d1ade` |
| `vendor/hyper/src/common/mod.rs` | 21 | `cf2ef9e25cde` |
| `vendor/hyper/src/common/task.rs` | 45 | `9d027a9c9c65` |
| `vendor/hyper/src/common/time.rs` | 79 | `6073390d9395` |
| `vendor/hyper/src/common/watch.rs` | 73 | `0530dbb42bdd` |
| `vendor/hyper/src/error.rs` | 679 | `ded435e7d5ea` |
| `vendor/hyper/src/ext/h1_reason_phrase.rs` | 221 | `296ecdcb206e` |
| `vendor/hyper/src/ext/informational.rs` | 86 | `83a9b2a9cd70` |
| `vendor/hyper/src/ext/mod.rs` | 295 | `5ecbf5bf2900` |
| `vendor/hyper/src/ffi/body.rs` | 302 | `614955cfca93` |
| `vendor/hyper/src/ffi/client.rs` | 274 | `f3e7b519d972` |
| `vendor/hyper/src/ffi/error.rs` | 96 | `dd269cd749cf` |
| `vendor/hyper/src/ffi/http_types.rs` | 703 | `a1addfbd00ec` |
| `vendor/hyper/src/ffi/io.rs` | 198 | `94455b259bdb` |
| `vendor/hyper/src/ffi/macros.rs` | 53 | `8e1fe5824429` |
| `vendor/hyper/src/ffi/mod.rs` | 99 | `80639b0ff428` |
| `vendor/hyper/src/ffi/task.rs` | 549 | `feb1a51ed32b` |
| `vendor/hyper/src/headers.rs` | 159 | `43305ee388d5` |
| `vendor/hyper/src/lib.rs` | 139 | `a3405c478bc3` |
| `vendor/hyper/src/mock.rs` | 235 | `8b455312be74` |
| `vendor/hyper/src/proto/h1/conn.rs` | 1530 | `c8ad31c4039a` |
| `vendor/hyper/src/proto/h1/decode.rs` | 1236 | `cea41a3b77ce` |
| `vendor/hyper/src/proto/h1/dispatch.rs` | 808 | `073a2bf94418` |
| `vendor/hyper/src/proto/h1/encode.rs` | 660 | `0ec088e6d766` |
| `vendor/hyper/src/proto/h1/io.rs` | 967 | `d83c99b59dc2` |
| `vendor/hyper/src/proto/h1/mod.rs` | 113 | `179bbde1399e` |
| `vendor/hyper/src/proto/h1/role.rs` | 3098 | `210ad28ef2e3` |
| `vendor/hyper/src/proto/h2/client.rs` | 746 | `1112af53bdb5` |
| `vendor/hyper/src/proto/h2/mod.rs` | 446 | `1791ddbf5599` |
| `vendor/hyper/src/proto/h2/ping.rs` | 510 | `180dfd283127` |
| `vendor/hyper/src/proto/h2/server.rs` | 545 | `7cdf48b9b873` |
| `vendor/hyper/src/proto/mod.rs` | 73 | `075880551f7a` |
| `vendor/hyper/src/rt/bounds.rs` | 109 | `3c75b9039a57` |
| `vendor/hyper/src/rt/io.rs` | 405 | `096573f6f586` |
| `vendor/hyper/src/rt/mod.rs` | 48 | `db927b56ddc6` |
| `vendor/hyper/src/rt/timer.rs` | 127 | `14e28bb4f46d` |
| `vendor/hyper/src/server/conn/http1.rs` | 551 | `f040ce86f0ea` |
| `vendor/hyper/src/server/conn/http2.rs` | 312 | `6bebea3f057d` |
| `vendor/hyper/src/server/conn/mod.rs` | 20 | `b2393dc0d68c` |
| `vendor/hyper/src/server/mod.rs` | 9 | `ffe7729eba25` |
| `vendor/hyper/src/service/http.rs` | 65 | `74b6a556f77b` |
| `vendor/hyper/src/service/mod.rs` | 30 | `de143e994e00` |
| `vendor/hyper/src/service/service.rs` | 112 | `ad742e754791` |
| `vendor/hyper/src/service/util.rs` | 82 | `7d2fcf870172` |
| `vendor/hyper/src/trace.rs` | 128 | `a766c472433c` |
| `vendor/hyper/src/upgrade.rs` | 407 | `02107e8607fa` |
