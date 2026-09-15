# Protocol quality — the plan after Programme 2

Programme 2 (`PROTOCOL_ROADMAP.md`) took every protocol through an eight-point rubric by hand,
once. It found ~200 defects, and the classes that recurred are the useful output: fail-open
defaults, narrowing casts, tests that encode the bug, advertised knobs that do nothing, bounds
decided by configuration rather than by the answer, unbounded pre-auth input, and clients that
reach the vendor instead of the target.

This file is what comes next. Two principles decide what is on it:

1. **A ratchet beats a pass.** A pass finds today's instances; a ratchet finds the next one the
   day it is written. Every class Programme 2 found by hand that *can* be mechanised should be,
   and the baseline may only shrink.
2. **An independent oracle beats self-agreement.** The BLE HID descriptors, the GATT UUID keying
   and the bcdHID assertion were all green because the only parser that disagreed lived on
   someone else's machine. The cheapest way to find that class is to put an independent decoder
   in the test — and one already exists for ~109 of these protocols (see the pcap oracle).

Tick a box when the item is **verified**, not when it is written. Where an item has a number,
re-derive it before and after; the "starting point" figures below are from 15 September 2026
and drift.

---

## Starting point (measured 15 September 2026)

| Measure | Value | How derived |
|---|---|---|
| Server maturity | 39 Beta · 118 Experimental · 0 Stable | `.state(` scan over `src/server/**/actions.rs` |
| Client maturity | 1 Beta · 97 Experimental | same, `src/client` |
| Servers with an LLM path | 140 | `call_llm`/`llm_client` in `mod.rs` |
| … of which log `decision=` | 113 (**27 missing**) | `decision=` literal in `mod.rs`/`actions.rs` |
| Servers with a connection cap | **2** | `MAX_CONN`/`max_connections` token |
| TCP accept-loop servers without any read/idle timeout token | **18 of 32** | `TcpListener` in `mod.rs`, no `timeout(`/`IDLE`/`READ_TIMEOUT` |
| Servers registering peer handles (`[ message ]`/`[ disconnect ]`) | 32 | `peer_support::` in `mod.rs` |
| Clients registering a command channel (`[ send ]`) | 99 | `register_command_channel(` in `mod.rs` |
| `get_dependencies()` overrides | **1** | grep |
| Hand-rolled control-character filters still in the tree | 25 | `is_ascii_control`/`is_control()` outside `utils::sanitize` |
| `#[ignore]` in `tests/**/*.rs` | 248, **105 with no reason** | grep |
| Fixed `sleep(Duration::from_secs(N))` in e2e tests | 279 | grep |
| Files that print `SKIP` and pass | 19 | grep |
| Fuzz targets | **0** (no `fuzz/`, no `proptest`) | — |
| Panic hook | TUI terminal-restore only — **a panic inside `tokio::spawn` is logged nowhere in `--mcp` mode** | `src/tui/event_loop.rs:73` |
| `[profile.release] overflow-checks` | **off** (wraps silently where it ships; panics where you test) | `Cargo.toml` |
| Protocols with a verified Wireshark dissector entry | 109 | `src/tui/wireshark.rs` |
| Protocols with a real third-party client binary **already on this machine** | 67 of 100 checked | `which` over a candidate table |
| Blocking CI test job | 6 protocols | `.github/workflows/ci.yml` |

Two of those numbers correct `CLAUDE.md`: command-channel adoption is 99 clients, not "`tcp` and
`telnet`"; and the panic hook exists but restores the terminal rather than logging.

---

## Tier 0 — central mechanisms that protect every protocol at once

These are one file each and change the failure mode for all 140 servers. Do these first; they
are cheaper than any per-protocol sweep and several of the sweeps below become ratchets only
once these exist.

- [ ] **A logging panic hook, installed in every mode.** `std::panic::set_hook` that writes the
  panic message, the thread and a backtrace at ERROR through `tracing`, then chains to the
  default hook. *Why:* `tokio::spawn` swallows panics, and Programme 2 found three families
  (`block_on` in USB, `blocking_lock` in SMB, `.unwrap()` inside spawned client tasks) where the
  task died, the server stayed `Running`, the log showed success and the peer hung. The TUI's
  hook only restores the terminal; `--mcp` has none. *Verify:* a test that panics inside a
  spawned connection task and asserts the message reaches `netget.log`. *Effort:* S.

- [ ] **`overflow-checks = true` in `[profile.release]`.** *Why:* STOMP's `content-length:
  18446744073709551615` panicked in every test build and wrapped harmlessly where it shipped —
  backwards from where you want to find it. With the panic hook above, an overflow in production
  becomes a logged task death instead of a silent wrong answer. Measure the cost on a hot path
  (`tuntap`, `rawip`) before and after; expect <2%. *Effort:* S.

- [ ] **`log_template.rs` strips control characters from every interpolated value.** *Why:*
  LLDP, CDP and HSRP each fixed injection into their own log lines locally; the shared template
  renderer is the one place that protects all 140 at once, including the ones nobody has looked
  at. A forged newline in a log line is a forged log entry. *Verify:* a test renders a template
  with `\r\nERROR fake` in a field and asserts one line. *Effort:* S.

- [ ] **Migrate the 25 hand-rolled control-character filters to `utils::sanitize`, then ratchet.**
  *Why:* six protocols had six semantics before the shared module existed, and the one told to
  be copied (`whois`) had none. The ratchet fails on any new `is_ascii_control`/`is_control()`
  filter outside `src/utils/`. *Effort:* M (mechanical, but each site needs the right variant —
  `line_field` vs `strip_controls` is a correctness choice, not style).

- [ ] **Per-connection tasks tracked and aborted on `stop_server`.** A `JoinSet` on the server's
  entry in `AppState`, every `tokio::spawn` for a connection registered into it, aborted by
  `remove_server`. *Why:* `CLAUDE.md` records that `stop_server` does not cancel in-flight
  connections. A stopped server that keeps answering is worse than one that refuses to start.
  *Verify:* start, connect a peer that never sends, stop, assert the peer reads EOF within 1s.
  *Effort:* M — the registration is one line per protocol, but there are ~140 of them; do the
  shared accept-loop helpers first so most inherit it.

- [ ] **`decision=` tagging on the 27 servers that have an LLM path and no tag.** The list is
  in the measurement script; `ftp`, `http`, `telnet`, `udp`, `mqtt`, `ntp` and `tftp` are the
  ones that matter most because they are Beta. *Why:* without the tag, a backend outage and a
  model refusal are the same log line, and `grep decision=fail_closed` — the one diagnostic this
  repo teaches — finds nothing. *Verify:* make it a ratchet: any `mod.rs` containing `call_llm`
  must contain `decision=`. *Effort:* M.

- [ ] **Silent-on-failure becomes metadata, not prose.** `ProtocolMetadataV2::failure_mode`
  (`Answers(WireFailure category)` / `DeliberatelySilent { reason }`), declared by every server.
  *Why:* `CLAUDE.md` carries a list of 20 deliberately-silent protocols that was found wrong
  in both directions (NDP logged its own transmit failure as `model_silent`; several BLE
  profiles were on it by family membership rather than by decision). A declaration is
  checkable; a paragraph is not. *Verify:* `wire_failure_test` reads the declaration and asserts
  a `DeliberatelySilent` server writes nothing on the LLM-error path, and an `Answers` server
  writes its category. *Effort:* M.

## Tier 1 — independent oracles

- [ ] **The pcap oracle: every frame NetGet emits is dissected by tshark without error.** A test
  helper that captures what a server writes (from the test's own socket — no capture privilege
  needed), wraps it in a synthetic pcap with the right link type and port, runs
  `tshark -r x.pcap -V -d <decode-as>` using the *already-verified* names in
  `src/tui/wireshark.rs`, and fails on `[Malformed Packet]` or any `Expert Info (Error)`.
  *Why:* this is an independent decoder for **109 protocols at once**, written by people who
  have read every RFC, and it is already installed. It would have caught the HID descriptors,
  the transposed pad bytes, the big-endian bcdHID, CDP's EtherType-as-length, and every "wrong
  flags bit" class Programme 2 had to reason about by hand — those are exactly what a dissector
  flags. *Verify:* the helper itself gets a test that feeds it a deliberately truncated DNS reply
  and asserts it fails. *Effort:* M for the helper, then S per protocol to adopt (one call at
  the end of an existing e2e test). Start with the 39 Beta servers — a Beta whose frames tshark
  rejects is misrated.

- [ ] **Fuzz targets for every pre-authentication decoder.** `cargo-fuzz` with `libfuzzer`,
  one target per hand-written parser, run in a nightly CI job for a fixed budget. Candidates,
  in order of exposure: bencode (`utils/bencode.rs`), AMQP field tables, NATS/STOMP framing,
  SNMP BER, RADIUS attributes, DNS wire format, LLDP/CDP/HSRP TLV walkers, Modbus/CoAP/GTP/M3UA
  headers, NDEF records, USB/IP + CTAPHID assembly, HID report maps, SIP/RTSP line parsing, the
  NFS RPC guard, SMB2 headers, TDS/MySQL/PostgreSQL startup packets. *Why:* all six stack
  overflows and every "bound the declared size" defect were found by *reasoning*, one at a
  time. A fuzzer finds the seventh while you sleep, and a crash is `SIGSEGV`, which nothing
  else in this suite can observe. *Verify:* each target runs 60s clean locally before merging;
  the CI job uploads any crash artefact. *Effort:* L overall, S per target once the harness
  exists. **This is the single largest robustness gap in the tree.**

- [ ] **Property tests for every codec that has both directions.** `proptest` round-trips:
  `decode(encode(x)) == x` for arbitrary `x`, and `encode` output length ≤ the declared bound.
  *Why:* `m3ua` had `MAX_MESSAGE_LEN` on decode and nothing on encode; the property would have
  said so. *Effort:* M.

- [ ] **Hard-fail the 19 skip-when-missing gates.** Each prints `SKIP` and returns `Ok(())`, so
  a runner without the binary is a silent pass. `npm` is the shape to copy. The binaries are on
  this machine for `oci_registry` (crane), `maven` (mvn), `websocket` (websocat), `radius`
  (radclient) and most of the rest — see the 67-of-100 table. *Why:* a real client behind a
  skip gate is not evidence, and four Beta ratings were one line away from being defensible.
  *Effort:* S each. Add the binaries to `ci.yml`'s `registry-audit` job as you go.

- [ ] **A second independent client for every Beta rating that rests on one.** One client can
  agree with one bug: HTTP with `curl` *and* Python `http.client`; DNS with `dig` *and* the
  system resolver; Redis with `redis-cli` *and* `redis-rs`. *Why:* two implementations that
  disagree with each other but agree with NetGet is the strongest evidence short of the spec.
  Cheap where the binary is already installed. *Effort:* S each.

## Tier 2 — resource bounds, swept and ratcheted

Programme 2 bounded what it found. These are the bounds every connection-oriented server should
declare, whether or not anyone has looked at it.

- [ ] **Idle and first-read timeouts on the 18 TCP servers without any.** `cassandra`, `db2`,
  `etcd`, `kafka`, `m3ua`, `mcp`, `memcached`, `mssql`, `mysql`, `nfs`, `postgresql`, `redis`,
  `smb`, `svn`, `tls`, `tor_relay`, `torrent_peer`, `zookeeper`. `whois`'s
  `FIRST_QUERY_READ_TIMEOUT`/`IDLE_AFTER_REPLY_TIMEOUT` pair is the shape. *Why:* a peer that
  connects and says nothing holds a connection, a task and a state entry forever, and 128 of
  them is a free denial of service on a server with no connection cap. *Verify:* ratchet — any
  `mod.rs` with `TcpListener` must reference a timeout constant. *Effort:* M.

- [ ] **A connection cap on every accept loop.** Two servers have one. A shared
  `accept_bounded(listener, max)` helper in `server/` that every accept loop calls, refusing
  past the cap with the protocol's own "busy" vocabulary where one exists (SMTP 421, HTTP 503,
  RESP `LOADING`) and a close where none does. *Why:* the NFS guard chose 256 for a reason;
  nothing else chose anything. *Effort:* M.

- [ ] **Max message / frame size declared in metadata and asserted by a test.** Every server
  states its bound (`ProtocolMetadataV2::max_inbound_bytes`), and a generic test sends
  bound+1 and asserts a refusal before any model call. *Why:* the unbounded-body class
  (kubernetes, ipp, openai, openapi, ollama, modbus's accumulator, fido2's assembly) was found
  seven times by hand. A declared bound is greppable; an undeclared one is a guess. *Effort:* M.

- [ ] **A soak test per protocol family.** Ten thousand short connections against a running
  server; assert `AppState` connection count returns to zero and RSS is flat. *Why:* the
  `connectionless` sweep and the peer-handle removal paths are the kind of thing that leaks one
  entry per connection and is invisible until production. *Effort:* M for the harness.

- [ ] **Stop releases the port, every protocol.** A generic test: start on port 0, read the
  bound port, stop, bind that port again within 1s. *Why:* `register_server_task` is
  "required for `stop_server` to actually release the socket" and adoption has never been
  measured. *Effort:* S — one parametrised test over the registry.

## Tier 3 — evidence and maturity

- [ ] **Define Stable, then earn it for five protocols.** Zero are Stable and three have lost
  it. The bar should be written down before anything is promoted: (1) two independent
  third-party clients complete a real session, hard-fail when absent; (2) the pcap oracle is
  green; (3) a fuzz target exists and has run; (4) every declared bound has a test; (5) the
  protocol's CLAUDE.md has been verified claim-by-claim within the last pass; (6) no
  `#[ignore]` in its suite. Candidates with the cheapest path: `whois`, `gopher`, `finger`,
  `dns`, `http`, `tcp`, `ntp`, `redis`. *Effort:* M each.

- [ ] **Beta for every Experimental server whose real client is already installed.** The
  67-of-100 table names them. For each: is the existing test driven by that client (not
  `reqwest`, not a hand-rolled decoder)? If yes and not skip-gated, it is Beta now and
  under-rated. If no, the client is one test away. `radius` (radclient), `snmp` variants,
  `ldap` (ldapsearch), `ipp` (ipptool), `sftp`, `rsync`, `smb` (smbclient), `nfs`
  (`mount_nfs` needs root — record that), `syslog` (logger), `arp` (arping), `icmp` (ping).
  *Why:* the August 29 lesson — five protocols sat at Experimental with the evidence already
  in the tree because nobody re-read it. *Effort:* S–M each.

- [ ] **Install the missing clients and do the same.** `brew install` covers most of the
  33: `mosquitto`, `nats-server`+`nats`, `kcat`, `mongosh`, `etcd`, `zookeeper`, `subversion`,
  `mercurial`, `grpcurl`, `coap-client` (libcoap), `mbpoll`, `stuntman`, `coturn`, `lldpd`,
  `wakeonlan`, `bitcoin` (large), `aria2`. Record each in `ci.yml` as it lands. *Effort:* S
  each; the install is the work.

- [ ] **Client maturity.** 97 of 98 clients are Experimental and the rubric barely touched
  the question of what a client *proves*. Define the bar: a client is Beta when it completes a
  real session against a **real third-party server** (not NetGet's own — that is circular in
  the other direction), hard-failing when the server is absent. Local servers are cheap for
  most: `redis-server`, `postgres`, `mysqld`, `mosquitto`, `nats-server`, `nginx`, `bind`,
  `sshd`, `vsftpd`. *Effort:* L overall.

- [ ] **Re-derive the "not promoted" list in `CLAUDE.md`.** It has been wrong in both
  directions three times. A script that, for each Beta server, prints the third-party crate or
  binary its evidence uses, whether the test is `#[ignore]`d, and whether the dependency is
  `optional = true` — so the list is generated, not maintained. *Effort:* S.

## Tier 4 — usability for the model

Robustness against a hostile peer is half of it. The other half is whether the model can drive
the protocol at all — and the mock never tells you, because the mock is scripted.

- [ ] **A real-model eval per protocol, tracked as a number.** `./test-e2e.sh --use-ollama`
  exists. Make it a harness: for each protocol, N canonical operator instructions ("serve a
  page saying hello", "answer example.com with 1.2.3.4", "accept user alice, reject everyone
  else"), run against a small local model, drive with the real client, score pass/fail.
  Publish the per-protocol success rate in `PROTOCOL_ROADMAP.md` and re-run nightly. *Why:*
  this is the only measurement of "the model can use it" in the tree, and every prompt-quality
  change is currently unmeasured. It is also the measurement that catches an action
  description the model misreads — the `send_first` "not typically needed" wording, the
  `{{event.xid}}` placeholder — before a user does. *Effort:* L for the harness; S per
  protocol to add instructions.

- [ ] **Action and parameter description ratchet.** Every parameter: a description of at
  least one sentence, a `type_hint` from the known set, an `example`; every action: an
  example its executor accepts (exists), a `log_template`. Every event: every field the model
  is told to echo back is a field the event carries. *Why:* the model chooses from the
  description alone. *Effort:* S — extend `executable_examples_test`.

- [ ] **Every startup example actually starts.** `startup_examples_validation_test` checks
  shape. A second test spawns each example against the mock and asserts `ServerStatus::Running`
  within 5s, for every protocol that needs no system library. *Why:* ten BLE examples were
  inert for a reason no shape check can see. *Effort:* M.

- [ ] **Peer handles on every connection-oriented server.** 32 have them; the dashboard greys
  out `[ message ]`/`[ disconnect ]` on the rest and says why. *Why:* the operator cannot reach
  a parked peer without one, and manual-first is the dashboard's whole premise. *Effort:* S
  each — the `whois` diff is ~40 lines.

- [ ] **`get_dependencies()` on the 17 device protocols and every system-library one.** One
  override exists. *Why:* the exclusion-with-install-hint mechanism is fully plumbed and does
  nothing; a model offered `nfc` on a machine with no reader gets a runtime error instead of
  never being offered it. *Effort:* S each.

## Tier 5 — test-suite hygiene

The suite is the evidence. Where it lies, the ratings lie.

- [ ] **A reason on every `#[ignore]`.** 105 of 248 have none. Each becomes
  `#[ignore = "claims the BLE adapter"]` or is un-ignored. *Verify:* ratchet — bare
  `#[ignore]` fails. *Effort:* S, mechanical.

- [ ] **Replace the 279 fixed `sleep(from_secs(N))` in e2e tests with a condition.**
  `wait_for_mocks`, `wait_for_any`, `wait_for_stat`, `wait_for_log`. *Why:* this is where every
  load-flake came from, and `m3ua`'s suite went 6s → 0.46s when its three sleeps became waits.
  *Verify:* ratchet — no `sleep(Duration::from_secs(` in `tests/server` or `tests/client`
  outside a helper. *Effort:* M, mechanical but wide.

- [ ] **`verify_mocks` after every `with_mock`.** A test that configures a mock and never
  verifies it asserts nothing about the model. *Verify:* source ratchet. *Effort:* S.

- [ ] **The blocking CI test job covers every protocol that needs no system library.** It runs
  6 of 116. `modbus` and `coap` are Beta on evidence CI never executes. Split into a matrix by
  family so each job stays under the runner's memory; keep the six-protocol job as the fast
  gate. Make `registry-audit` blocking once it is green three runs in a row. *Effort:* M.

- [ ] **`single-feature` over all 116, not 14.** *Why:* it is the only check that finds an
  under-declared dependency, and it costs one `cargo check` each. Run nightly. *Effort:* S.

- [ ] **Five consecutive full sweeps at `--test-threads=100`, any failure investigated.** Not
  labelled — investigated. The tuntap/rawip 60s failures turned out to be build contention;
  the doh client failures turned out to be the keychain. Both were "flaky" until someone
  looked. *Effort:* M.

## Tier 6 — documentation truth

- [ ] **Every backtick path in a `CLAUDE.md` must exist.** A ratchet over
  `src/**/CLAUDE.md` and `tests/**/CLAUDE.md`: any `` `path/to/file.rs` `` or
  `` `fn_name` `` that names a file must resolve. *Why:* the `remote` test doc described three
  btleplug tests that did not exist; the `openai` server doc said "no LLM prompting" beside a
  table of LLM call budgets. A path check catches the first class outright. *Effort:* S.

- [ ] **Test counts in docs are generated or absent.** "22 tests" was wrong in three files.
  Either a script updates them or the docs stop stating them. *Effort:* S.

- [ ] **Per-protocol CLAUDE.md claim audit as a recurring pass, not a one-off.** Programme 2
  verified them once. Schedule it: every protocol's two docs re-read against source every
  quarter, with the drift recorded. *Effort:* L, recurring.

- [ ] **Correct `CLAUDE.md` on the two counts this file measured**: command-channel adoption is
  99 clients; the panic hook does not log. *Effort:* S.

## Tier 7 — the classes Programme 2 found, as ratchets

Each of these was found by hand in several protocols. Each can be a shrink-only source scan.

- [ ] **Affirmative default on a status field.** `unwrap_or("90")`, `unwrap_or(200)`,
  `unwrap_or(true)`, `unwrap_or(0)` on a field named `status`/`code`/`result`/`sw1`/`sw2`/
  `ok`/`success`/`allowed`. *Why:* NFC and usb-smartcard each had it in two layers. *Effort:* S.

- [ ] **Configuration-decides-the-bound.** Any budget or escalation gate that inspects
  `event_handlers` rather than the handler's *result*. *Why:* tuntap. Hard to scan generically;
  scan for the idiom (`handlers.iter().any(` inside a budget path). *Effort:* S.

- [ ] **Vendor-default fallback in clients.** Any client wrapping an SDK whose `remote_addr` can
  be empty without an `Err`. *Why:* DynamoDB, then openai, then openapi. *Effort:* S.

- [ ] **Recursive decoder without a depth counter.** The crude scan found 973 self-referencing
  functions; refine to functions that both recurse and take `&[u8]`/`&mut Reader`, and require
  a `depth` parameter or a `MAX_*_DEPTH` in scope. *Why:* six stack overflows, and `catch_unwind`
  cannot see a seventh. *Effort:* M.

- [ ] **Example drifts from const.** Any `get_startup_examples()` containing a hex literal longer
  than 16 bytes that is not produced by `hex::encode(CONST)`. *Why:* four of five HID profiles.
  *Effort:* S.

---

## Suggested sequencing — Programme 3

**Week 1 — Tier 0.** Seven items, all central, all small or medium. After this, a task panic is
visible, an overflow is a logged death rather than a wrong answer, and a log line cannot be
forged from any protocol. Nothing per-protocol yet.

**Week 2 — the pcap oracle and the fuzz harness.** Build both helpers, adopt the oracle in the 39
Beta suites, land the first ten fuzz targets. This is where the next HID-descriptor-class defect
gets found, and it is found by tshark rather than by an agent reasoning about bytes.

**Weeks 3–4 — the sweeps, as agents in worktrees.** Timeouts, connection caps, `decision=` tags,
sanitizer migration, peer handles, `#[ignore]` reasons, sleep → wait. Each is mechanical per
protocol and each ends in a ratchet, so the batch structure from Programme 2 applies directly:
one agent per family, reporting across boundaries, merged with `--no-ff`.

**Week 5 — evidence.** Hard-fail the 19 skip gates, promote what the 67 installed clients
already prove, install the 33 missing ones, define Stable and earn it for five.

**Ongoing — the real-model eval.** Start it in week 2 with five protocols and grow it; it is the
only number in this repository that says whether the model can drive the thing at all.

---

## Done

Move items here with the date and the commit or PR that verified them.
