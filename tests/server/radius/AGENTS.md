# RADIUS server test strategy

Three layers, weakest evidence first, so it is obvious which claims rest on what.

## Layer 1 — codec against literal bytes (`e2e_test.rs`, no network, no LLM)

Every hex literal in that file was produced or checked with **Python `hashlib`** — a
genuinely separate MD5 implementation — before it was written down. Two of them are the
published RFC 2865 §7.1 example pair, so they are also independent of me.

| Test | What it pins |
|---|---|
| `decodes_the_rfc_2865_7_1_access_request` | header + TLV parsing against the RFC's own packet |
| `decrypts_the_rfc_2865_5_2_user_password` | §5.2 unhiding yields `arctangent` |
| `produces_the_rfc_2865_7_1_access_accept_byte_for_byte` | **the Response Authenticator**, against the RFC's published reply |
| `the_response_authenticator_depends_on_the_secret` | the secret is actually an input (a stub that ignored it would pass the test above) |
| `unhides_a_multi_block_password_matching_an_independent_implementation` | block chaining past 16 octets, both directions |
| `rejects_password_lengths_the_rfc_forbids` | 0, non-multiple-of-16, >128 |
| `verifies_an_accounting_request_and_rejects_a_forged_one` | RFC 2866 §3 authenticator, and that a wrong secret is refused |
| `drops_malformed_datagrams` | short packet, length mismatch, zero-length attribute (an infinite loop in a naive parser), attribute past the end |
| `ignores_padding_beyond_the_declared_length` | RFC 2865 §3 padding rule |

LLM calls: **0**. Runtime: milliseconds.

## Layer 2 — end to end through the real binary (`e2e_test.rs`, LLM mocked)

The netget binary is started with a mock Ollama, and a raw UDP socket plays the NAS. The
request is the RFC §7.1 packet, so what the model receives is checkable.

| Test | LLM calls | What it pins |
|---|---|---|
| `access_request_accepted_when_the_model_says_so` | 2 | Accept path end to end: echoed identifier, verifying Response Authenticator, Reply-Message, Framed-IP-Address, Session-Timeout, and that `"Login-User"` is really translated to the integer 1 rather than passed through as text |
| `fails_closed_when_the_model_returns_nothing` | 2 | **The OAuth2 regression test.** See below |
| `model_denial_is_distinguishable_from_silence` | 2 | An explicit denial is Access-Reject with the model's own reason, logged `decision=model_reject`, and asserted *not* to be logged as fail-closed |
| `access_challenge_state_round_trips` | 3 | Challenge → State on the wire → NAS echoes it → the model sees it and accepts |

Total: **9 LLM calls**, inside the ~10 budget.

### On `respond_with_actions_from_event`

Three of the four use it. The UDP rule in the root `CLAUDE.md` exists because a static mock
with a hardcoded transaction ID makes the client time out; here the mechanism matters for a
slightly different reason worth writing down:

**RADIUS's identifier and Request Authenticator are echoed by the server, not by the model.**
`RadiusProtocol::for_request` carries them, exactly as NTP carries the origin timestamp, so
they are not action parameters and a mock cannot get them wrong. What the mock *does* derive
from the event is the **decision** — the accept branch fires only if `user_name` is `nemo`
and `password` is `arctangent`. That means a server that mis-decrypted the password, or
handed the model the wrong user, turns the test red. A fixed `send_access_accept` blob would
have asserted nothing about either.

`access_challenge_state_round_trips` is the clearest case: the second reply is an accept only
if the State the server put on the wire came back and reached the model.

### The OAuth2 regression test

`fails_closed_when_the_model_returns_nothing` is the test OAuth2 never had. The mock answers
the `radius_access_request` event with `[]` — the model responded, but with no protocol
action. That is precisely the shape that, in OAuth2, fell through to a hardcoded access token.

It asserts four things:

1. The reply code is **Access-Reject**, not Access-Accept.
2. The whole packet equals a literal computed by Python `hashlib`, so the denial is also
   correctly signed — a denial a client discards as corrupt is a timeout, not a denial.
3. The Reply-Message is `"Access denied: no authorization decision was produced"`, which the
   model has no way to produce accidentally.
4. The log contains `decision=fail_closed_no_action` and **does not** contain
   `decision=model_reject`.

Point 4 is the one that is easy to leave out and is the actual regression: OAuth2's failure
was not only that silence became approval, but that silence and denial were indistinguishable.
`model_denial_is_distinguishable_from_silence` is its mirror image and asserts the inverse
pair, so neither label can quietly start covering both cases.

## Layer 3 — a real, independent client (`real_client_test.rs`)

**FreeRADIUS 3.2.10 `radclient`** (`brew install freeradius-server`; GPL, invoked as a
subprocess, never linked). This is the only peer here that NetGet did not write.

It is a strong oracle specifically because it verifies the Response Authenticator and says so:

```text
Reply verification failed: Received Access-Accept packet from home server 127.0.0.1
port 18120 with invalid Response Authenticator!  (Shared secret is incorrect.)
```

That was confirmed by hand against a deliberately zeroed authenticator before these tests were
written — `radclient` exits 1 and never prints "Received Access-Accept". So a green run means
the MD5 is genuinely right, not merely self-consistent.

| Test | LLM calls | What it pins |
|---|---|---|
| `freeradius_radclient_accepts_our_access_accept` | 2 | An accept a foreign client accepts, with the attributes decoded by its own dictionary |
| `freeradius_radclient_sees_a_valid_reject_when_the_model_is_silent` | 2 | The fail-closed reject is *also* well-signed and readable by a foreign client |

### FreeRADIUS is a hard requirement, not a nicety

**Both tests FAIL when `radclient` is absent.** `require_radclient()` returns `Err`; it does
not print SKIPPED and return `Ok(())`, which is what it used to do. That single line is the
whole reason RADIUS may be rated `Beta` — the root `CLAUDE.md` is explicit that a
skip-when-missing gate is a silent pass rather than evidence, and it names four protocols
(`kubernetes`, `oci_registry`, `maven`, `websocket`) held back from `Beta` for exactly this.
`tests/server/npm/e2e_test.rs` is the shape copied.

`require_radclient()` additionally runs `radclient -v` and asserts the banner, so a broken or
architecture-mismatched install fails there with its own message rather than three assertions
later as an unexplained timeout. It asserts the banner rather than the exit status because
some builds exit non-zero from `-v` while still printing it.

**So FreeRADIUS must be installed wherever the `radius` feature's suite runs:**

```bash
brew install freeradius-server            # macOS
sudo apt-get install -y freeradius-utils  # Debian/Ubuntu — radclient only, not the daemon
```

Searched in order: `/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, then `$PATH`.

**This costs the CI gate nothing.** `radius` is not in `CI_FEATURES`
(`tcp,http,dns,udp,redis,mcp-stdio`), and the file is `#![cfg(feature = "radius")]`, so the
blocking `test` job never compiles it. Nor does `single-feature`, whose list is
`tcp udp dns http redis tls telnet ftp ldap syslog ntp tftp socks5 mqtt`. The only
`--all-features` job, `registry-audit`, names its `--test` targets explicitly and `server` is
not among them. If `radius` is ever added to `CI_FEATURES`, the workflow must install
`freeradius-utils` the same way it already installs `bind9-dnsutils` for the `dig` test.

Note that `radclient` sends a **Message-Authenticator** (attribute 80) by default. NetGet
neither verifies nor returns one; `radclient` 3.2.x does not require it, so the exchange
succeeds. This is stated in `src/server/radius/CLAUDE.md` and in `metadata()` rather than
papered over: a NAS configured to demand Message-Authenticator will reject our replies.

## What is deliberately not tested, because it is not implemented

CHAP, MS-CHAP, EAP and Message-Authenticator. There is no test asserting they work, because
they do not, and `metadata()` says so. Adding a test that merely asserts "CHAP-Password
appears in the event as hex" would read as coverage of CHAP and is worse than nothing.

## Running

```bash
CARGO_TARGET_DIR=/tmp/tgt ./cargo-isolated.sh test --no-default-features --features radius \
    --test server::radius -- --test-threads=100
```
