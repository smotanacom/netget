# Shared prompt evaluation — 2 October 2026

Full 105-case matrix: **455/520 (87.50%) before**, **457/520 (87.88%) after**. One NTP case was unavailable because its client needs privileged port 123; all other 104 cases have five fresh trials.

Both phases use `llama3.1:8b`, sampler seed 42, model-default temperature, and identical cases, instructions, probes and eval harness. The model digest is `46e0c10c039e019119339687c3c1757cc81b9da49709a3b3924863ba87ca666e` (Q4_K_M, macOS arm64). The resumed baseline, after phase and fixed replay record Ollama 0.35.0; the original preflight recorded the same model digest but did not retain its daemon version.

The before source is `d34d91c09ee1731ec13eb3977c17344aeda95497`; the after source is `3e2d24bd935fe822296bad2962c82675dfcd820b`. The latter includes the shared action-only network prompt, final-round-only execution and verbatim template rendering, plus adjacent dashboard/browser/HTTP body-bound fixes and stricter NSQ hexadecimal message IDs. Native bridge metadata changes preserve the Ollama request format. All protocol action definitions used by the matrix are unchanged. This is a comparison of those source revisions; the fixed-request replay isolates their captured prompt differences. Later added protocols and eval-harness changes do not extend this historical 105-case matrix.

A separate final-integration harness correction replaces NSQ publish acceptance’s shutdown-log check with successful client exit and no `E_PUB_FAILED`, retaining the probe and bounds. The pinned go-nsq producer can exit after its waitgroup completes but before logging `exiting router`. Frozen baseline `nsq/accept-publish` run 1 exhibits this false failure: the client exits 0 without a timeout or publish error, and NetGet executes `send_nsq_ok`, but the log line is absent. The archived 4/5 before and 5/5 after scores remain unchanged. This accounts for one reported gain; the aggregate two-trial difference is not an isolated model-accuracy improvement. `final-harness-scope.json` retains the exact observation and scope.

The original full before invocation was interrupted after 32 completed cases. The resumed measurement preserves all original trials in `baseline-interrupted.json`. Two original trials logged backend transport failures (Gemini ask-for-input run 3 and Beanstalkd queue-statistics run 1); all cases in those two protocols were repeated with five trials. The laptop also slept repeatedly from 14:34:42 UTC until full wake at 17:01:45 UTC during WHOIS/Gopher trials; every case in those two protocols was repeated too. The original trials remain visible. The continuation also repeats Finger’s first two cases, which are excluded in favor of the preserved original cases. `baseline.json` records the exact report source selected for every case; no individual trial was selected by outcome.

The continuing baseline and complete after phase execute the already-built `run-eval.sh` test binary with its exact model, seed, run count and protocol environment. This avoids rebuilding while preserving the scoring implementation. Original command:

```sh
./run-eval.sh --model llama3.1:8b --runs 5 --seed 42 --out <phase-output>
```

Inspect `provenance/`, `harness-equivalence.json`, `host-sleep-audit.json` and `backend-availability-audit.json` for source, binary and harness hashes, actual commands, selected protocols, times, sleep/wake timestamps and backend-error excerpts. Complete phase logs are retained as hash-indexed gzip files in `raw-logs/`. Owned `caffeinate -dims` prevented idle sleep through the resumed phases; the final completion audit found no sleep/wake events from continuation start through replay completion.

The before measurement spans an interruption, sleep-affected protocol repeats and a backend recovery. Runtime comparisons below use recorded trial durations, not a claimed uninterrupted full-run wall time. Native verification, WASM and release builds may overlap the full phases; CPU load and ephemeral ports/IDs were not controlled across the complete phases, so aggregate score and latency changes are descriptive. The fixed-request replay waits for and holds the shared Rust build slot, and complements them.

The selected full-trial logs record 424 builtin tool executions before and 0 after. Logged model request attempts total 1114 before and 721 after; session-oriented cases can ask more than one question. `trial-log-audit.json` retains the per-trial counts, missing-request markers, tool names and report selection.

The report's `model_output` field is empty for 7 before trials and 0 after trials. Full logs have no completed-response marker for 2 before trials and 0 after trials. These are separate from backend outages: some empty report fields still have completed model replies in the raw log, while some clients leave before a reply. The old Nostr `nak` probes retain their original short client deadlines; repeated prompt rounds can exceed them. All five trials and their original client bounds are preserved, and those wire failures remain scored. The aggregate measures this client/protocol behavior, not only completed model answers.

## IMAP greeting actions

| Case | Before: greeting actions per trial | After: greeting actions per trial |
| --- | --- | --- |
| imap/list-folders | [5, 5, 5, 5, 5] | [1, 1, 1, 1, 1] |
| imap/inbox-count | [1, 1, 1, 1, 1] | [1, 1, 1, 1, 1] |

The deterministic regressions in [tests/llm_bridge_test.rs](../../tests/llm_bridge_test.rs) verify that an unadvertised tool cannot execute or commit its draft actions, a tool round’s actions are replaced by the final response, and exhausting tool rounds fails without committing draft actions.

## Captured request replay

The IMAP connection, LOGIN and LIST; HTTP request; and Telnet connection and message requests were captured from both exact binaries. Independent repeat captures were byte-identical within each variant. Every patched prompt omits the builtin tools section and `generate_random`; each is 6,629 characters shorter. Original request bytes, plain prompts, manifest hashes, and all raw replay responses are retained.

Each fixed request was replayed three times against the same model and seed after both full phases completed, pairing variants per request and repetition and alternating first/second order. Replay changes only `stream` to `false` to retain one complete response. The tool/action counter finds the first answer-shaped JSON value; it does not substitute for NetGet’s response validation.

| Event | Before: tools per replay | After: tools per replay | Before: actions per replay | After: actions per replay |
| --- | --- | --- | --- | --- |
| imap_connection | [0, 0, 0] | [0, 0, 0] | [1, 1, 1] | [2, 2, 2] |
| imap_auth | [0, 0, 0] | [0, 0, 0] | [1, 1, 1] | [3, 3, 3] |
| imap_command | [1, 1, 1] | [0, 0, 0] | [1, 1, 1] | [1, 1, 1] |
| http_request | [1, 0, 0] | [0, 0, 0] | [1, 1, 1] | [1, 1, 1] |
| telnet_connection_opened | [1, 1, 1] | [0, 0, 0] | [1, 1, 1] | [1, 1, 1] |
| telnet_message_received | [1, 1, 1] | [0, 0, 0] | [0, 0, 0] | [1, 1, 1] |

Raw-response review counts ten builtin-tool objects before and zero after. The baseline LIST and Telnet connection/message replies attach `generate_random` despite needing only fixed responses. This count includes three malformed baseline Telnet message envelopes: their string-concatenation syntax is not valid JSON, so the counter finds only a nested tool object. The replay also exposes remaining instruction-following problems: both HTTP variants and the patched Telnet reply drop the requested quotes and punctuation. IMAP LOGIN replies include conditional prose or extra actions. These are inference observations, not passing native wire tests; `replay-response-review.json` records every finding and response hash. Baseline IMAP connection and HTTP response text also varies between repetitions despite identical payloads and seed; no cause is assumed.

| Event | Before mean / median (s) | After mean / median (s) |
| --- | ---: | ---: |
| imap_connection | 6.768 / 5.037 | 3.505 / 2.376 |
| imap_auth | 10.394 / 9.376 | 3.537 / 2.959 |
| imap_command | 5.277 / 3.654 | 3.792 / 2.956 |
| http_request | 4.464 / 1.325 | 2.617 / 1.204 |
| telnet_connection_opened | 5.609 / 3.502 | 3.401 / 2.391 |
| telnet_message_received | 4.761 / 3.643 | 2.185 / 1.638 |

## Case changes

| Case | Before | After |
| --- | --- | --- |
| bolt/count | 5/5 | 3/5 |
| bolt/syntax-error | 3/5 | 0/5 |
| coap/text-resource | 5/5 | 0/5 |
| dict/unknown-word | 0/5 | 1/5 |
| ftp/banner | 3/5 | 5/5 |
| ftp/working-directory | 0/5 | 5/5 |
| gearman/unknown-function-fails | 0/5 | 5/5 |
| gemini/not-found | 5/5 | 1/5 |
| http/json-status | 5/5 | 2/5 |
| ipp/printer-stopped | 4/5 | 5/5 |
| ldap/single-person | 2/5 | 1/5 |
| ldap/two-people | 0/5 | 5/5 |
| modbus/illegal-address | 4/5 | 5/5 |
| mqtt/refuse-client-id | 4/5 | 5/5 |
| mqtt/retained-message | 4/5 | 5/5 |
| nostr/accept-film-note | 2/5 | 5/5 |
| nostr/refuse-adverts | 1/5 | 0/5 |
| nostr/serve-notes | 4/5 | 5/5 |
| nsq/accept-publish | 4/5 | 5/5 |
| pop3/message-count | 5/5 | 1/5 |
| pop3/message-subject | 4/5 | 5/5 |
| prometheus/queue-depth-gauge | 5/5 | 0/5 |
| smtp/refuse-other-domain | 1/5 | 5/5 |
| telnet/answer-command | 3/5 | 5/5 |
| websocket/echo | 4/5 | 0/5 |

Summed recorded trial time: 9898.9s before, 5060.9s after. Mean trial: 19.04s before, 9.73s after. Median trial: 12.46s before, 7.65s after.

The complete after report is preserved here as `after.json` and published as `../latest.json`; `comparison.json` contains every case, both trial latency summaries, and all gains and regressions. Failures remain in the generated report and raw trial records.

Run `python3 eval-results/2026-10-02-shared-prompts/verify_export.py` from the repository root to verify artifact hashes, original trial selection, report comparison, compressed raw logs, per-trial model/tool audits, captured request bytes and all paired replay payloads/responses without contacting a model.
