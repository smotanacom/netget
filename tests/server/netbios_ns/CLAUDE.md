# NetBIOS-NS server test strategy

Two layers, weakest evidence first, so it is obvious which claims rest on what. There is no
third layer, and the reason is the whole story of this protocol's maturity rating — see
"Why there is no real-client layer" at the bottom.

Everything lives in `e2e_test.rs`. `mod.rs` declares it behind `#[cfg(all(test, feature =
"netbios-ns"))]`, and `tests/server/mod.rs` declares the directory — without that last line the
suite compiles nothing and reports nothing, which is the repo's largest historical test hole.

## Layer 1 — codec against bytes a third party produced (no network, no LLM)

The two request literals are **packet captures**, not hand-written hex:

```bash
tcpdump -i lo0 -n -X 'udp port 137' &
nmblookup -U 127.0.0.1 NETGETTEST     # -> SAMBA_NAME_QUERY
nmblookup -A 127.0.0.1                # -> SAMBA_NODE_STATUS_QUERY
```

Samba 4.24.6 wrote those 50 octets each; NetGet did not. That is what makes "the first-level
name encoding is right" a claim rather than a hope, and it is the one part of NBNS that
implementations reliably get wrong. Both literals were additionally reproduced with an
independent Python encoder before being written down, and they match byte for byte.

| Test | What it pins |
|---|---|
| `first_level_encoding_matches_the_published_wildcard` | `'*'` + 15 NUL -> `CKAAAA…`, the canonical published encoding and what `nmblookup -A` really sent. Also that `pad_netbios_name` NUL-pads the wildcard rather than space-padding it |
| `first_level_encoding_round_trips_every_octet` | all 256 values both directions; every output character in `A..=P`; a `'Q'` and a short field are refused |
| `the_suffix_is_the_sixteenth_octet_and_never_part_of_the_name` | `FILESERVER<0x00>` and `FILESERVER<0x20>` differ in exactly the last two characters (`AA` vs `CA`); a 16-character name is an error |
| `decodes_the_name_query_samba_sent` | the real query, field by field — and that the question starts at offset **12**, so a 16-octet header would fail |
| `decodes_the_node_status_query_samba_sent` | the real NBSTAT query, and that the wildcard arrives as `"*"` and not `"*\0\0…"` |
| `encodes_a_positive_name_response` | header flags, one NB RR, 6 octets per address, the group bit and the ONT bits; an empty address list is refused |
| `encodes_a_negative_response_as_a_null_rr` | RFC 1002 §4.2.14 shape, RD echoed, and that RCODE 0 is refused (it would be a positive answer carrying nothing) |
| `encodes_a_node_status_response_with_a_raw_name_list` | `NUM_NAMES` + 18 per name + 46 of statistics; the name list is **raw** 16 octets, not encoded 32; UNIT_ID is the MAC and every other statistic is zero |
| `parses_and_formats_mac_addresses` | the formatted-string contract, both separators, and two rejections |
| `refuses_datagrams_that_are_not_answerable_requests` | a response (`R=1`), truncation, header-only, a wrong first-label length, a compression pointer, and an over-long datagram |

LLM calls: **0**. Runtime: milliseconds.

The `R=1` case is worth calling out: a server that answers a *response* is a reflector, and
UDP source addresses are trivially spoofed.

## Layer 2 — end to end through the real binary (LLM mocked)

A raw UDP socket plays the querier. The first request in each test is the captured Samba
datagram, replayed byte for byte, so what the model receives is checkable against a real
client's idea of the protocol.

| Test | LLM calls | What it pins |
|---|---|---|
| `answers_queries_and_registrations_the_model_decides` | 4 | positive answer, refusal, and a registration refusal on one server |
| `node_status_lists_the_names_the_model_invented` | 2 | the NBSTAT path, the raw name list and the MAC |
| `an_llm_failure_produces_no_datagram_at_all` | 2 | **the silence test** |
| `model_chosen_silence_is_distinguishable_from_an_outage` | 2 | its mirror image |

Total: **10 LLM calls**, at the ~10 budget.

### On `respond_with_actions_from_event`, and why the transaction id is not echoed by the mock

The root `CLAUDE.md` rule for UDP protocols is that a static mock with a hardcoded transaction
id makes the client time out. Here the mechanism is different and the rule still applies for a
different reason, which is worth writing down.

**NBNS's transaction id, opcode and question name are echoed by the server, not by the model** —
`NetbiosNsProtocol::for_request` carries them, exactly as `radius` carries the identifier and
Request Authenticator. They are not action parameters, so a mock cannot get them wrong, and a
model cannot poison a cache by getting them wrong either. The tests still assert the echo
directly (`header.trn_id == 0x047f`, and the answer RR's NAME field compared byte-for-byte
against the question's) so a regression that dropped the echo turns them red.

What the mocks *do* derive from the event is the **decision**:

- the query rule answers positively only when `event["name"] == "NETGETTEST"`, so a server that
  mis-decoded the first-level encoding produces a refusal and the assertions fail;
- the node status rule names `WORKGROUP` only when `event["name"] == "*"`, so a server that
  failed to trim the wildcard's NUL padding puts `UNEXPECTED` on the wire instead.

Both queries in the first test are answered by **one** rule branching on the event. Two rules
on the same event id would be first-match-wins: the first would answer both and the second
would report zero calls. That is the most common mocking mistake in this repo.

### The silence tests are the point of this suite

`an_llm_failure_produces_no_datagram_at_all` configures a mock for the *startup* instruction
only. `netbios_name_query` then matches no rule, the mock answers HTTP 500, and `call_llm`
returns `Err` — the same shape as a backend outage. It asserts:

1. **No datagram arrives at all** within 8 seconds (`expect_silence`, which fails loudly and
   prints the hex of anything that does arrive). A fabricated NBNS answer is cached by the
   querier and redirects that host's traffic for the TTL, so "answer something" is not an
   option here the way a 503 is for HTTP.
2. The log carries `decision=fail_closed_llm_error`.
3. The log does **not** carry `decision=model_silent` or `decision=model_reject`.

Point 3 is the one that is easy to leave out and is the actual regression risk. On this
protocol every silent path is byte-identical on the wire, so if the log conflates them there is
no way at all — not from a capture, not from the client — to tell a working deny-by-default
server from a broken one. `model_chosen_silence_is_distinguishable_from_an_outage` asserts the
inverse pair (`decision=model_silent` present, no `decision=fail_closed`), so neither label can
quietly start covering both cases.

`expect_silence` proving a negative is inherently a timeout, so it is the one place in this
suite that waits on the clock rather than on a condition. Eight seconds is well past the
LLM-failure path's retry loop as configured by the harness; if it ever becomes flaky the fix is
to wait for the `decision=` log line *first* and only then assert the socket is empty.

## What is deliberately not tested, because it is not implemented

WINS name-table state, conflict detection, NAME RELEASE / REFRESH / WACK, redirects, name
compression, the datagram service (UDP 138) and the session service (TCP 139). There is no test
asserting any of them, because a test that merely asserted "the datagram is dropped" would read
as coverage of a feature that does not exist. `src/server/netbios_ns/CLAUDE.md` lists them.

## Why there is no real-client layer

`nmblookup` **is** installed (`/opt/homebrew/bin/nmblookup`, Samba 4.24.6) and **is** a genuine
third-party client — it produced the literals in layer 1. It cannot drive layer 2, and this was
verified rather than assumed:

- `nmblookup --help` and its man page offer no port option at all. `-U` takes an address only;
  `-U 127.0.0.1:13137` fails to parse the target name.
- `--option="nbt port=13137"` is *accepted* by the smb.conf parser — `nbt port` is a real Samba
  parameter, for the source4 NBT **server** — and is ignored by this client. Confirmed by
  capture: with that option set the query still went to `127.0.0.1.137`, while a socket bound to
  13137 received nothing.
- Binding UDP 137 needs root on macOS.

So a real-client exchange requires a privileged run. The root `CLAUDE.md` is explicit that an
`#[ignore]`d root test is not evidence and that a skip-when-missing gate is a silent pass, so
neither was written. The protocol is rated **Experimental** accordingly, and
`metadata().notes` states the reason.

If a privileged lane ever exists, the test to write is: start NetGet on 137 with a static
handler, run `nmblookup -U 127.0.0.1 SOMENAME` and `nmblookup -A 127.0.0.1`, and assert on
`nmblookup`'s own stdout — that is the only thing that would prove the *response* direction,
which is currently unvalidated by anything independent.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features netbios-ns \
    --test server netbios_ns -- --test-threads=100
```

Note `--test server netbios_ns`, not `--test server::netbios_ns::e2e_test`: `tests/server.rs`
is the only test target here and `netbios_ns` is a name filter.
