# NetBIOS-NS client tests — strategy

`./cargo-isolated.sh test --no-default-features --features netbios-ns --test client netbios_ns \
    -- --test-threads=100`

**11 tests, 11 passing.** 8 pure (no network, no LLM), 3 that spawn the real binary.

The layering here is not decoration: the strength of each layer is exactly what the protocol's
`DevelopmentState` turns on, and the file is ordered strongest-evidence-first so that is hard
to misread.

## Layer 1 — the encode direction, pinned by a third party

`SAMBA_NAME_QUERY` and `SAMBA_NODE_STATUS_QUERY` were captured off loopback for this suite:

```bash
tcpdump -i lo0 -n -w nbns.pcap 'udp port 137' &
nmblookup -U 127.0.0.1 NETGETTEST     # -> SAMBA_NAME_QUERY        (QTYPE NB)
nmblookup -A 127.0.0.1                # -> SAMBA_NODE_STATUS_QUERY (QTYPE NBSTAT)
```

Samba 4.24.6. `tcpdump` needs no root here because the account is in `access_bpf`.

The client's own encoder must reproduce both **byte for byte**, transaction id included.
NetGet wrote none of those bytes, so this is genuine independent evidence — for one direction.
Both literals also match, after the transaction id, the separately captured ones in
`tests/server/netbios_ns/e2e_test.rs`: two independent runs of a third-party client rather than
one transcription.

**Generate these constants, never retype them.** Both were hand-typed once and both were wrong
— one by a byte too many, one by a byte too few, because a 30-character run of `A`s is not
something a human transcribes reliably. The failure then reads as an encoder bug: the assertion
message shows two nearly identical hex strings and invites you to go looking in `wire.rs`,
where nothing is wrong. Extract them from the pcap with a script and write them into the file.

Layer 1 also pins the two traps that cost the server side a debugging pass, on the client's
main path this time:

- `the_node_status_query_this_client_builds_is_byte_identical_to_sambas` asserts the encoded
  wildcard label is `CKAAAA…` and **asserts it is not** `CKCACA…`, the space-padded form. Every
  node status query uses the wildcard.
- The same test asserts `*` round-trips back as `"*"` and not `"*\0\0…"` — `trim_end()` does
  not strip NULs.

## Layer 2 — the decode direction, against NetGet's own encoders

Same-project evidence, and labelled as such in the file. It shows the two halves agree, not
that either matches RFC 1002.

The node status test is the one that matters: it asserts the **same machine name at two
different suffixes stays two entries**, that neither carries its suffix inside the name string,
that `group` is true only for the workgroup entry, and that `active` is false for one entry
so the flag decoding is not vacuously "all true".

## Layer 3 — end to end through the real binary, LLM mocked

Two responders, on purpose:

| Test | Responder | What it proves |
|---|---|---|
| `resolves_a_name_against_netgets_own_nbns_server` | NetGet's NBNS server | the two halves interoperate over a real socket |
| `parses_a_node_status_reply_and_discards_a_mismatched_transaction_id` | raw UDP stand-in | the things a cooperating server will never do |

**A cooperating server cannot test the important behaviour.** The stand-in answers an NBSTAT
question correctly and answers everything else with a deliberately **wrong transaction id**, so
one client exercises both the node status parse and the discard-plus-timeout path.

That test is the load-bearing one, and the assertion is indirect by necessity: the responder
*did* answer the `MISSINGHOST` query. A client that failed to match on the transaction id would
raise `netbios_name_response`, so the `netbios_query_timeout` rule would report zero calls and
`verify_mocks` would fail. The `WARN` line is also asserted directly via `output_contains`.

`the_stand_in_responder_behaves_as_the_tests_assume` is a fixture guard with NetGet entirely
out of the picture. Without it, a bug in the fixture reads as a bug in the client.

### Mock budget and the traps observed

**9 LLM calls total**, under the ~10 guideline:

| Test | calls |
|---|---|
| `resolves_a_name_against_netgets_own_nbns_server` | 2 server (`open_server`, `netbios_name_query`) + 3 client (`open_client`, connected, name response) |
| `parses_a_node_status_reply_and_discards_a_mismatched_transaction_id` | 4 client (`open_client`, connected, node status answer, timeout) |

The node-status and timeout cases are deliberately merged into one client. Split, they would
need two more `open_client` + connected pairs and push the file over budget for no extra
coverage.

Traps that apply here specifically:

- **One rule that branches, never two rules on one event.** The server side answers
  `netbios_name_query` with a single `respond_with_actions_from_event` that returns a positive
  response for `NETGETTEST` and `name_not_found` otherwise. Two rules would be first-match-wins:
  the first answers everything and the second reports zero calls.
- **Echo the suffix from the event**, exactly as UDP-style protocols must echo transaction ids.
  `"suffix": e["suffix"]` means a server that mis-decoded the first-level encoding cannot
  produce the right value.
- **`and_event_data_contains` on an array matches the compact JSON form.** The node status
  assertion is `("names", "\"suffix\":32")` — that substring can only appear if the raw 16-octet
  name list was split into name and suffix rather than rendered as text. It is a suffix
  assertion disguised as a substring match, and worth keeping for that reason.
- **Never hardcode a transaction id.** They are random per query by design. Nothing in this
  suite asserts a specific one against the live client; the fixture echoes the id it received,
  or deliberately inverts it.

### The glob-import trap in this directory

Use explicit imports from `crate::helpers`, **not** `use crate::helpers::*`. The helpers module
contains a submodule literally named `netget`, and a glob import of it shadows the crate of the
same name, so `netget::client::netbios_ns::wire` stops resolving with E0659. The error names
the glob, but only after you have read past two screens of "ambiguous name".

## What this suite deliberately does not do

- **It does not drive `nmblookup` against a NetGet client or server.** `nmblookup` is hard-wired
  to UDP 137 (verified on the server side: no port option, and `--option="nbt port=…"` is
  accepted by the config parser and ignored by the client), and binding 137 needs root on
  macOS. An `#[ignore]`d root test is not evidence, so there is no such test rather than a
  skipped one.
- **It does not skip when a binary is missing.** There is nothing to skip: the third-party
  dependency is a *recorded capture*, not a binary that has to exist at run time. That is the
  one structural advantage this suite has over the four protocols in the root CLAUDE.md whose
  real-client tests silently pass when the client is not installed.

So the decode direction has never been checked against anything NetGet did not write, which is
why the client is rated **Experimental** and not Beta. Byte-identical queries are necessary and
not sufficient.

## Note on the whole-tree ratchets

`executable_examples_test::the_example_audit_has_something_to_inspect` fails at this feature set
and at every other narrow one — it asserts `checked > 900` examples, and a single-protocol build
compiles ~9. Its own module docs say to run it at `--all-features`. The test that actually
audits examples, `no_action_ships_an_example_its_own_executor_refuses`, passes here and at this
feature set it is checking precisely this protocol's examples.
