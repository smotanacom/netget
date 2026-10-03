# SSDP server test strategy

Two layers, weakest evidence first, so it is obvious which claims rest on what.
There is deliberately **no third layer**, and that absence is the whole reason
the protocol is `Experimental` — see "The layer that does not exist".

Everything lives in `e2e_test.rs`, declared from `tests/server/ssdp/mod.rs`.

## Layer 1 — codec against literal UDA 1.1 message text (no network, no LLM)

The M-SEARCH and NOTIFY literals in the file are the message shapes the UPnP
Device Architecture 1.1 specification prints in §1.3.2 and §1.2.2. They are
independent of this implementation in the only sense available: they were
written from the spec, not from the code's output.

| Test | What it pins |
|---|---|
| `parses_the_uda_msearch` | start line, case-insensitive header lookup, that `MAN`'s quotes survive, MX as a number |
| `parses_the_uda_alive_notify` | NOTIFY parsing, and that `headers` reaches the model as a **map** |
| `a_status_line_is_recognised_as_a_response_not_a_request` | `HTTP/1.1 200 OK` yields `method: None`, which is what stops `mod.rs` answering a response and starting a discovery loop |
| `refuses_datagrams_that_are_not_ssdp` | empty, blank-only, non-UTF-8, header with no `:`, non-request non-status start line, `SSDP/1.0` version, over-long |
| `tolerates_bare_lf_on_input` | real implementations are sloppy; we emit strict CRLF regardless |
| `mx_is_clamped_to_five_seconds` | §1.3.2's clamp, and that *absent* MX is distinct from MX 0 |
| `renders_the_mandatory_search_response_header_set` | the exact §1.3.3 header set, byte for byte, including the empty `EXT:` — then reparses it |
| `the_date_header_is_an_http_date` | `GMT`, not RFC 2822's `+0000` |
| `a_byebye_carries_only_the_four_headers_uda_allows` | §1.2.3, byte for byte |
| `the_mx_jitter_stays_in_range_and_is_not_degenerate` | the delay bound in both directions, and that 400 draws produce >20 distinct values |

LLM calls: **0**. Runtime: milliseconds.

### Why the jitter is tested by sampling, not by timing

A test that asserts "the response took at least N ms" is a flake at
`--test-threads=100`, and a test that asserts "at most N ms" would pass against
a constant. So `response_delay_bound_ms` is asserted directly in both
directions (cap wins / MX wins / cap 0 disables), and `response_delay_ms` is
drawn 400 times with two assertions: every draw is inside the bound, and the
draws take more than 20 distinct values. The second one is the important half —
a hardcoded `return 0` passes a bounds check alone, and the whole point of the
jitter is that it is not constant.

## Layer 1b — the executor, on the stateless registry instance

Dispatched on `SsdpProtocol::new()` (or `for_request`), no server involved.

| Test | What it pins |
|---|---|
| `header_injection_through_extra_headers_is_refused` | CR/LF in a value, in a plain string field, and a mandatory header name reused inside `extra_headers` |
| `a_location_that_is_not_a_fetchable_url_is_refused` | four shapes: not a URL, relative, `ftp:`, scheme-less |
| `an_unknown_nts_is_refused` | UDA defines three; a control point ignores anything else, so an unchecked one is a silent no-op |
| `st_is_echoed_for_a_concrete_search_but_never_for_a_wildcard` | the echo, **and** that `ssdp:all` / `upnp:rootdevice` are an error rather than a guess |
| `no_response_is_its_own_result_not_a_bare_no_action` | `Custom{ssdp_no_response}`, not `NoAction` |
| `the_executor_strips_location_from_a_byebye` | §1.2.3 enforced by the executor, not just by the renderer |

`header_injection_through_extra_headers_is_refused` is the one worth keeping.
In most protocols response splitting is an integrity bug; here it lets an
attacker's `LOCATION` be appended to an otherwise honest advertisement, and the
control point will fetch it.

## Layer 2 — end to end through the real binary (LLM mocked)

The netget binary is started with a mock Ollama and driven over a raw UDP
socket bound to 127.0.0.1.

| Test | LLM calls | What it pins |
|---|---|---|
| `answers_a_matching_search_and_stays_silent_for_a_mismatch` | 3 | the full §1.3.3 response for a match; **no datagram at all** for a mismatch; `decision=model_response` then `decision=model_reject`, and that neither is `fail_closed` |
| `stays_silent_but_logs_when_the_llm_fails` | 2 | **no datagram** on a backend outage, plus `decision=fail_closed_llm_error`, and that it is *not* recorded as `model_reject` |
| `an_inbound_notify_reaches_the_model_and_an_announcement_goes_out` | 2 | the `ssdp_notify` event fires and its `nts` reaches the model (the mock branches on it, so a server that mis-decoded the announcement answers `no_response` and the observer times out); `send_ssdp_notify` produces a real NOTIFY whose `HOST` names the group. Note that `nts` is the **only** inbound field under test — nothing asserts that `usn`, `nt`, `location` or `server` survived the trip |

Total: **7 LLM calls**, inside the ~10 budget. The seventh is the one the
failure test *wants* to fail: it declares no `ssdp_msearch` rule, so that request
reaches the mock, matches nothing and gets an HTTP 500 — a single attempt, since
the transport error propagates rather than being retried. It is a real request to
the mock and so it counts, but `verify_mocks()` asserts only the six *expected*
ones; the seventh is asserted through the `decision=fail_closed_llm_error` log
line instead.

### The searches are unicast, and that is not a shortcut

Every M-SEARCH in this suite is addressed straight at the server's own ephemeral
port on 127.0.0.1. UDA 1.1 §1.3.2 permits a unicast search, so these are real
M-SEARCHes — not a testing-only path. It is what makes the suite deterministic
and unprivileged: no port 1900, no real link, and **no dependence on the
multicast join having succeeded**.

That last point is the load-bearing one. Multicast on loopback is exactly the
kind of environment-dependent behaviour that turns into a test which passes on
a laptop and hangs on a runner.

### Testing the silence

Two of the three tests assert that **nothing** arrives, which needs saying
explicitly because a test that asserts an absence is easy to write badly.

Both use `ask(port, datagram, secs)`, which sends and then waits for a real
timeout on `recv_from`, returning `None` when nothing came. The waits are 6
seconds against a mock that answers immediately (or 500s immediately), so the
window is not marginal. And both pair the absence with a **log assertion**: an
absent datagram alone is indistinguishable from the server never having received
the request, which is precisely the silent-failure defect this protocol is
shaped to avoid. So each asserts the pair — nothing on the wire, and the
specific `decision=` token that explains it.

Each also asserts the *negative*: the mismatch test asserts no
`decision=fail_closed` appears, and the LLM-failure test asserts no
`decision=model_reject` appears. Without those, one label could quietly start
covering both cases, which is the OAuth2 failure in its silence-shaped form.

### On `respond_with_actions_from_event`

The two tests with an event rule derive the mock's answer from the event rather
than returning a fixed blob, and each does so for a reason that would otherwise go
untested. (`stays_silent_but_logs_when_the_llm_fails` has no event rule at all —
that is the point of it — and its startup answer is a fixed blob.)

* the search test branches on `st`, so a server that handed the model the wrong
  search target — or dropped it — turns the matching case into a refusal and
  fails the assertions;
* the notify test branches on `nts`, so a server that did not decode the
  inbound announcement answers `no_response` and the observer socket times out.

**One rule per event, branching inside it** — not two rules. Two rules on the
same event with no way to tell them apart is the standing mistake in this repo:
the first answers every occurrence and the second reports zero calls.

Unlike the UDP-family protocols, there is no transaction ID to echo, so the
`respond_with_actions_from_event` requirement in the root `CLAUDE.md` does not
bite here for its usual reason. SSDP's equivalent — the `ST` echo — is done by
the *server* (`resolve_st`), not by the model, so a mock cannot get it wrong;
that is asserted in Layer 1b instead.

### `notify_target`, and why the announcement test is not cheating

`an_inbound_notify_reaches_the_model_and_an_announcement_goes_out` points the
server's announcements at a socket the test owns, via the `notify_target`
startup parameter, rather than at `239.255.255.250:1900`.

That is not a shortcut around a broken path. Measured on macOS 27:

| Operation | bound `127.0.0.1` | bound `0.0.0.0` |
|---|---|---|
| join 239.255.255.250 | succeeds | succeeds |
| `sendto(239.255.255.250:1900)` | **`EADDRNOTAVAIL` (49)** | succeeds |

Loopback carries no multicast route, so the default send genuinely cannot work
from the address this suite binds. The parameter exists for that, and the test
still asserts the part that matters: the announcement is a real, well-formed
`NOTIFY * HTTP/1.1` whose `HOST` header names the multicast group **even though
the datagram went somewhere else** — because `HOST` describes the group the
announcement is about, not the socket it travelled over.

What is therefore *not* proven: that a datagram actually reaches the group on a
real link. Nothing in this suite proves that.

## The layer that does not exist, and why

There is no third-party-client layer, so **nothing here proves a real UPnP
control point accepts our responses**. That is the whole reason `metadata()`
says `Experimental` and its `e2e_testing` note says so in as many words.

It is not a gap that a dependency would close. Every Rust SSDP client surveyed
(2026-09) sends to the hardcoded group and cannot be aimed at a unicast
ephemeral port — `ssdp-client` 2.1.0 has the destination as an inline literal
and no parameter for it; `rupnp` 3.0 re-exports that same function; `ssdp` 0.7
has a `unicast()` API but filters loopback out of its socket list; `upnp-rs`
0.2 has a `search_once_to_device(SocketAddr)` that unconditionally
`join_multicast_v4`s the destination and so fails on `127.0.0.1`. The full
survey with sources is in `src/server/ssdp/CLAUDE.md`.

A server on 127.0.0.1:ephemeral cannot receive a datagram sent to the group
anyway, so this is structural.

The two remaining options are both explicitly *not* third-party evidence under
the root `CLAUDE.md`, and neither was taken:

* a hand-written M-SEARCH sender inside the test — the `dhcp` /
  `tests/helpers/usbip_client.rs` class, an independent reading of the spec
  rather than an independent implementation. (The Layer 2 tests already are
  this, and are counted as such: they are a raw `UdpSocket` writing bytes this
  repo composed.)
* vendoring `ssdp-client`'s search function with the destination
  parameterised, which stops being a third-party client the moment it is
  edited.

**No dependency was added.** Adding one would not have changed the rating.

## What is deliberately not tested, because it is not implemented

The device description document, SOAP control, SCPD, GENA eventing,
`BOOTID`/`CONFIGID` bookkeeping, spontaneous `ssdp:alive` on startup, and
duplicate-search suppression. `src/server/ssdp/CLAUDE.md` says so for all of
them; `metadata()` names only the first three, so it is the per-protocol doc and
not the metadata that is complete here.

The distinction worth holding is between *rendering* a header and *maintaining*
it. `renders_the_mandatory_search_response_header_set` does pass
`BOOTID.UPNP.ORG` through `extra_headers` and assert it comes out in the message,
which is a real test of `extra_headers`. What would be worse than nothing is a
test presenting that as coverage of the UDA versioning scheme: nothing here
increments a BOOTID across a reboot, or reads one, or acts on a peer's.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features ssdp \
    --test server -- ssdp --test-threads=100
```

Note `--test server -- ssdp`, not `--test server::ssdp::e2e_test`: the test
binary is `server`, and everything below it is a filter. The form written in
some other suites' docs is not a valid target and lists every binary instead.

Whole-tree ratchets that cover this protocol need a client compiled in as well,
or `event_action_declarations_test::the_client_audit_reports_its_own_coverage`
fails on an empty client registry — a feature-set artefact, not a defect:

```bash
./cargo-isolated.sh test --no-default-features --features ssdp,tcp \
    --test event_action_declarations_test --test event_emit_sites_test \
    --test startup_param_drift_test --test wire_failure_test \
    --test startup_examples_validation_test -- --test-threads=100
```

`executable_examples_test`'s own coverage assertion (`checked > 900`) needs
`--all-features`; its substantive test passes at any feature set.
