# `tests/eval/` — the real-model eval harness

**What it measures:** whether a real model, reading a protocol's own action
descriptions and parameter docs, can serve a plain-English operator instruction.

**Why nothing else measures that:** every other suite in this repository drives
a **mock** model whose answers the test author wrote. A mocked E2E proves the
plumbing carries a correct answer from the model to the wire. It says nothing
about whether the model can *produce* one — and "an LLM drives the protocol" is
NetGet's whole premise. Before this existed, every action description, every
`example` and every startup-parameter blurb in the tree was unmeasured.

Run it: `./run-eval.sh` (see `--help`). Output: `eval-results/latest.json` and
`EVAL_RESULTS.md`, regenerated together.

## This is not a gate, and must not become one

`tests/eval.rs` skips unless `NETGET_USE_OLLAMA=1`. It fails only when the
**harness** could not run — no Ollama, no model, no cases compiled in, or every
attempted case erroring before the model was asked. A low score is a finding
about an action description, not a broken build, and `NETGET_EVAL_MIN_RATE`
exists for anyone who wants a floor but is off by default.

The job is `.github/workflows/nightly-eval.yml`, deliberately outside `ci.yml`
and dispatched by hand rather than run on a cron.

## How it is put together

| File | What it owns |
|---|---|
| `case.rs` | `EvalCase`, `Probe`, `Expect`, and the `Independence` label |
| `suites.rs` | the instruction sets, one function per protocol, feature-gated |
| `probe.rs` | spawning the third-party client and reading it until idle |
| `classify.rs` | turning a failed run into a named diagnosis with evidence |
| `runner.rs` | the run loop, the repetitions, the scoring |
| `report.rs` | the JSON and the Markdown |

The server is started by `helpers::llm_live::LiveRequestTest`, which runs
`netget --server <proto> --port N "<instruction>"` — **no model call at setup**.
So the only unpredictable step in a run is the model answering the network
event. Setup correctness is a separate question with its own tests in
`tests/llm_live/`; chaining the two would make every failure ambiguous.

## The rules a new instruction set must obey

1. **Never name an action, a parameter or an event in an instruction.** Write
   what an operator would type. The moment an instruction says
   `send_http_response`, what is under test stops being the description and
   becomes the model's ability to copy.
2. **Drive it with a real third-party client where one exists**, and label the
   evidence honestly when one does not. `Probe::client` is a real client;
   `Probe::generic` is `nc` and says so in the results. A pass carried by a byte
   pipe and a pass carried by `dig` are not the same evidence, and this repo has
   been burned by conflating them before.
3. **Never skip silently.** A missing binary is `client-missing`; a protocol no
   installed client can reach is `EvalCase::unavailable` with the reason. A skip
   that reads as a pass is the exact defect `PROTOCOL_QUALITY.md` catalogues in
   the rest of the suite (19 files that print `SKIP` and return `Ok(())`).
4. **Check what the client observed**, not what netget logged — except for
   one-way protocols (syslog), where `Expect::executed_action` is the only
   observable. That check sees **only netget's `Executing action` lines**, never
   the whole log, and the reason is a false pass it already produced: netget
   dumps a rejected model reply into the log verbatim, so a whole-log search
   matched `{"type": "ignore_syslog_message"}` inside a reply that had been
   *thrown away*, and syslog scored 3/3 having executed nothing. The model
   naming an action and netget running it are different events.

## Determinism: a pinned seed, and still a rate

Every run passes `--llm-seed` (default 42; `--seed` / `NETGET_EVAL_SEED`, `none`
for an unpinned run) and, when asked, `--llm-temperature` (`--temperature` /
`NETGET_EVAL_TEMPERATURE`). netget sends both in Ollama's `options` object
(`src/llm/ollama_client.rs`, `SamplingOptions`), and sends neither when the
flags are absent — `tests/llm_sampling_options_test.rs` pins that from the wire.

**A pinned seed pins the sampler, not the run.** Ollama draws the same tokens for
the same prompt, but each run's prompt carries its own client port, connection
id and, for DNS/LDAP-style protocols, a random query or message id — one
differing token changes every token drawn after it. So each instruction still
runs N times (default 3) against a **fresh netget process and a fresh server**,
the published number is passes/runs with every verdict listed, and each case
reports `verdicts_agree` (every run reached the same verdict) and
`actions_agree` (every run executed byte-identical actions). The Markdown has a
Reproducibility section with the totals. `actions_agree` undercounts by design
for anything that must echo a random id.

`--out DIR` (`NETGET_EVAL_OUT_DIR`) writes the two artefacts into `DIR` instead
of over the committed baseline — use it for a one-protocol rerun.

## Traps already paid for

Each of these cost a debugging pass and every one presented as a model failure.

- **`nc` closes the socket when its stdin reaches EOF.** Writing the payload and
  dropping the handle hangs up before the model is asked: netget logged
  `TCP received 11 bytes` and `Connection closed` 300µs apart. `Probe::generic`
  sets `hold_stdin`, and `probe.rs` streams until idle instead of calling
  `wait_with_output`.
- **macOS `whois` segfaults when `-p` follows `-h`** — exit 139, zero bytes sent.
  No argument order reaches a loopback port, so whois is driven with `nc`.
  Worth knowing because `whois` is counted among the installed real clients in
  `PROTOCOL_QUALITY.md`.
- **The shared helpers' Ollama checks used to be too tight to survive
  back-to-back model calls, and that is fixed.** `check_ollama_available`
  (`tests/helpers/netget.rs`) built a fresh `reqwest::Client` and gave
  `http://localhost:11434/api/tags` **2 seconds**; `ensure_model_available` did
  the same with 5. Both paid the keychain cost of building the client and the
  mDNSResponder cost of `localhost`, and both ran while Ollama was still
  finishing the previous case — five of eleven cases in the first smoke run were
  refused against a healthy Ollama. They now share one client built through
  `client_for_endpoint` against `127.0.0.1` and allow `OLLAMA_PROBE_TIMEOUT`
  (`tests/helpers/common.rs`). `runner.rs` still polls `/api/tags` first, because
  a bound is one attempt and a sweep needs a poll.
- **`./test-e2e.sh --use-ollama` was broken** and is fixed in the same pass: it
  appended `-- --use-ollama`, which libtest rejects outright
  (`error: Unrecognized option: 'use-ollama'`), so the binary exited before any
  test ran. The env var was always the mechanism.
- **Never let a harness bug score against the model.** A bad regex in an
  `Expect` returns `HARNESS: …` and is classified as `harness_error`, not as a
  miss.

## The two findings the first sweep produced

Both are about *prompts*, which is what this harness exists to measure, and
neither is visible to a mocked test.

1. **`valid_actions_rejected_as_unparseable`, 29 of the first 30 runs.** The
   model named the right action with the right parameters and netget threw the
   reply away, because `ActionResponse::from_str` (`src/llm/actions/mod.rs`)
   strips a *leading* ``` fence and nothing trailing, then requires
   `serde_json::from_str` to consume the whole string — with no fallback before
   it bails with `Invalid JSON`. Small models append an explanation after the
   JSON constantly. Every failed run is therefore also asked a counterfactual —
   *would the first JSON value in this reply have executed?* — and the report
   carries both numbers. Without that, one defect hides every description
   problem behind a flat 0%.

   Related: `generate_with_format`'s `format` argument has exactly one caller
   (`conversation.rs:1642`) and it always passes `None`, so Ollama's
   JSON-constrained output mode is never used. That is deliberate per its own
   comment (some models do not support it), which is why the parser is the
   better fix — it also covers the Bridge and OpenAI backends.

2. **`copied_example_content`.** Told "serve a menu whose first item is labelled
   Welcome to NetGet", the model emitted all four items of `send_gopher_menu`'s
   declared `example` — "Welcome to the gopher hole", "About this server",
   "Files", "Search the archive" — and none of the requested label. This is the
   `{{event.xid}}` placeholder defect wearing better clothes: a placeholder at
   least looks wrong on the wire, whereas an example full of plausible prose
   does not, so it wins against the operator's own instruction.

   `classify.rs` names this automatically by reading the protocol's declared
   examples out of the registry and looking for their distinctive string values
   in the executed action — **excluding anything the instruction itself
   contains**, since a value the operator asked for is not evidence of copying.
   An `example` is the strongest prompt a protocol has; whatever it contains is
   what a small model will send.

## Adding a protocol

1. A function in `suites.rs` returning 3–5 `EvalCase`s, `#[cfg(feature = "…")]`.
2. Add the protocol to `ALL_PROTOCOLS` in `run-eval.sh` (it validates against
   that list before spending a minute on a build).
3. Validate the client invocation against a bare listener **before** running the
   eval — confirm it connects and sends bytes. Finding out through a 90-second
   model call is how the `whois` and `nc` traps above were found, and each cost
   a full pass. Note that server-speaks-first protocols (mysql, ftp) send
   nothing to a silent listener; that is expected, not a probe bug.
