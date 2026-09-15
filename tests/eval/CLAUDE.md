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

The nightly job is `.github/workflows/nightly-eval.yml`, deliberately outside
`ci.yml`.

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
   one-way protocols (syslog), where `Expect::in_server_log` is the only
   observable and the runner waits for the needle rather than sleeping.

## Non-determinism: why the score is a rate

netget passes exactly one option to its Ollama backend — `num_predict`
(`src/llm/ollama_client.rs`). There is **no temperature, no seed, no top-p**,
and no CLI flag that sets one, so sampling runs at whatever the model's
Modelfile says and the same instruction genuinely produces different actions run
to run.

Pinning the seed would be better and is a one-field change in the `options`
object plus a flag; until it exists, each instruction runs N times (default 3)
against a **fresh netget process and a fresh server** — no conversation history,
no server memory, no connection state carried between runs — and the published
number is passes/runs with every individual verdict listed.

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
- **The shared helpers' Ollama checks are too tight to survive back-to-back
  model calls.** `check_ollama_available` (`tests/helpers/netget.rs`) builds a
  fresh `reqwest::Client` and gives `http://localhost:11434/api/tags` **2
  seconds**; `ensure_model_available` does the same with 5. Both pay the
  keychain cost of building the client and the mDNSResponder cost of
  `localhost`, and both run while Ollama is still finishing the previous case.
  Five of eleven cases in the first smoke run were refused against a healthy
  Ollama. `runner.rs` waits for `/api/tags` with its own long-lived client
  first; the real fix belongs in those helpers.
- **`./test-e2e.sh --use-ollama` was broken** and is fixed in the same pass: it
  appended `-- --use-ollama`, which libtest rejects outright
  (`error: Unrecognized option: 'use-ollama'`), so the binary exited before any
  test ran. The env var was always the mechanism.
- **Never let a harness bug score against the model.** A bad regex in an
  `Expect` returns `HARNESS: …` and is classified as `harness_error`, not as a
  miss.

## Adding a protocol

1. A function in `suites.rs` returning 3–5 `EvalCase`s, `#[cfg(feature = "…")]`.
2. Add the protocol to `ALL_PROTOCOLS` in `run-eval.sh` (it validates against
   that list before spending a minute on a build).
3. Validate the client invocation against a bare listener **before** running the
   eval — confirm it connects and sends bytes. Finding out through a 90-second
   model call is how the `whois` and `nc` traps above were found, and each cost
   a full pass. Note that server-speaks-first protocols (mysql, ftp) send
   nothing to a silent listener; that is expected, not a probe bug.
