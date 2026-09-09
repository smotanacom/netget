# LDAP Protocol Implementation

**Status**: `DevelopmentState::Beta`. The evidence is `tests/server/ldap/e2e_test.rs`, which
drives the server with **ldap3** — a real, independent LDAP client — through bind, search, add,
modify and delete. It is not `#[ignore]`d and does not skip when anything is missing, so it runs
on every pass. Not Stable: filters and scope are parsed but not evaluated, and no third-party
client has been taken through SASL or StartTLS because neither exists here.
(This line said `Experimental` while the code said `Beta`; the code was right.)
**Privilege**: `PrivilegeRequirement::PrivilegedPort(389)` — 389 is below 1024 and is where
every LDAP client looks by default, so the preflight check in `server_startup.rs` fires rather
than letting the bind fail with a bare EPERM.

LDAPv3 (RFC 4511) over TCP, with hand-written ASN.1 BER coding — no LDAP crate. That makes this
the highest-risk parsing in the file/directory protocol group, and the section on decoding
below is the important part of this document.

## Fail-closed

LDAP decides who is who, so silence must never be consent. In `LdapSession::respond_via_llm`:

| Situation | Wire result | Logged decision |
|---|---|---|
| Model returns a BindResponse with resultCode success | that response | `decision=model_accept` |
| Model returns a BindResponse with any other code | that response | `decision=model_reject` |
| Model returns no usable action, bind | **invalidCredentials (49)** | `decision=fail_closed_no_action` |
| Model returns no usable action, add/modify/delete | **unwillingToPerform (53)** | `decision=fail_closed_no_action` |
| Model returns no usable action, search | empty-but-successful result set | `decision=fail_closed_no_action` |
| LLM call errors | **unavailable (52)**, or **busy (51)** when overloaded | `decision=fail_closed_llm_error` |

Two things make the distinction structural rather than a matter of reading bytes back:

1. `respond_via_llm` returns a `Decided` alongside the step, so the bind path can only report
   `model_accept`/`model_reject` for a response the **model itself** produced. Inferring it from
   the encoded resultCode cannot tell a model's refusal from the server's substituted one, and
   conflating those is exactly what turned an outage into an approval in OAuth2.
2. The LLM-failure codes (51/52) are deliberately different from every per-operation default.
   `unavailable` is never a success, so a backend outage can never be mistaken for a valid
   empty answer — which is the one thing the search default could otherwise be confused with.

`self.authenticated` is state the model is *shown*, not a gate NetGet enforces: the model
decides what a bound-or-not session may do. Nothing in `actions.rs` can synthesise a successful
BindResponse; only `ldap_bind_response` with an explicit `result_code` of 0 produces one.

## Connection tracking

Peers are registered with `add_connection_to_server` in the accept loop and retired with
`close_connection_on_server` when the session ends, and `record_bytes` keeps the counters and
`last_activity` current on every read and write. LDAP is connection-oriented and so does **not**
declare `.connectionless()`; the 10-second idle sweep therefore does not touch these entries,
which makes the explicit close the only thing that retires one. Before this the server
registered nothing at all: the dashboard rail showed an LDAP server with no peers while clients
were bound to it, and no connection-scoped scheduled task could be created.

## No storage

There is no directory. A bind is granted or refused by the model; a search returns the entries
the model names; add, modify and delete are acknowledged and change nothing here, because
there is nothing here to change. Whether a search after an add agrees with it is the model's
memory to keep — say so in the instruction if you care.

## Decoding attacker-controlled BER

**The invariant: no index or range is used before it has been checked against the remaining
buffer.** `read_ber_element` is the only place raw input is sliced. It validates that an
element's claimed length actually fits and hands back a `value` slice that is already in
bounds; every parser above it walks `BerElement`s and never indexes the wire buffer itself.

The previous implementation did the opposite — it indexed and sliced using lengths read
straight off the wire:

```rust
let op_start = msg_start + id_bytes;
let op_tag = data[op_start];                                   // panic
let dn = String::from_utf8_lossy(&data[dn_data_start..dn_data_start + dn_bytes]); // panic
let pwd = &data[pwd_start..pwd_start + pwd_bytes];             // panic
```

Concretely, the seven-byte message `30 82 00 01 02 01 01` walked `op_start` to exactly
`data.len()` and panicked on `data[op_start]`. A panic in a connection task is silent: the task
dies, the client sees a closed socket, and the server keeps reporting `Running`.

Also bounded now:

- **Message size** — `MAX_LDAP_MESSAGE` (1 MiB). A long-form BER length can promise 4 GiB.
- **Filter nesting** — `MAX_FILTER_DEPTH` (32). `render_filter` recurses over client-supplied
  structure, so an unbounded renderer overflows the stack on a deliberately deep filter.
- **Integer width** — `ber_integer` rejects INTEGERs wider than 8 bytes and sign-extends
  properly, rather than shifting an arbitrary number of bytes into an `i32`.

Verified by fuzzing a running server (2026-08-05): truncated headers, lengths pointing past the
buffer, a 4 GiB long-form length, empty DNs, truncated attribute lists, a 200-deep nested
filter, and 40 random-garbage messages. No panic; the server answered a valid bind afterwards.

## Message framing

Messages are framed by their BER length and reassembled across reads (`ldap_message_len` plus
a persistent buffer). The old loop treated one `read()` as exactly one message, which is wrong
in both directions:

- `ldapsearch` pipelines bind and search into one segment — the second message was discarded
  and the client hung.
- Any message split across TCP segments, or larger than the 8 KiB buffer, was rejected as
  malformed.

## Operations

| request | tag | event | response tag |
|---|---|---|---|
| BindRequest | 0x60 | `ldap_bind` | BindResponse 0x61 |
| SearchRequest | 0x63 | `ldap_search` | SearchResultEntry 0x64 + SearchResultDone 0x65 |
| ModifyRequest | 0x66 | `ldap_modify` | ModifyResponse 0x67 |
| AddRequest | 0x68 | `ldap_add` | AddResponse 0x69 |
| DelRequest | 0x4A | `ldap_delete` | DelResponse 0x6B |
| UnbindRequest | 0x42 | `ldap_unbind` | none (RFC 4511 forbids one) |
| anything else | — | — | protocolError under the matching response tag, or ExtendedResponse 0x78 |

**Add, modify and delete were dead.** The three actions `ldap_add_response`,
`ldap_modify_response` and `ldap_delete_response` were declared, had working executors and were
documented — but no request parser and no event ever produced them. `ldapadd` got a
protocolError, and worse, that error was encoded with the *BindResponse* tag whatever the
request was, so a client decoding a search or add reply saw a protocol violation rather than
the protocolError it was meant to see. Both are fixed: the operations are parsed and raised as
events, and `response_tag_for` picks the tag matching the request.

## Events

Six, each advertising exactly the actions that can answer it:

| event | data | actions |
|---|---|---|
| `ldap_bind` | `message_id`, `version`, `dn`, `password`, `auth_type` | `ldap_bind_response`, `close_connection` |
| `ldap_search` | `message_id`, `base_dn`, `scope`, `filter`, `attributes`, `authenticated`, `bind_dn` | `ldap_search_response`, `close_connection` |
| `ldap_add` | `message_id`, `dn`, `attributes`, `authenticated`, `bind_dn` | `ldap_add_response`, `close_connection` |
| `ldap_modify` | `message_id`, `dn`, `changes`, `authenticated`, `bind_dn` | `ldap_modify_response`, `close_connection` |
| `ldap_delete` | `message_id`, `dn`, `authenticated`, `bind_dn` | `ldap_delete_response`, `close_connection` |
| `ldap_unbind` | `bind_dn` | none, declared with `.with_no_actions()` |

`ldap_unbind` used to carry `.with_actions(vec![])`, which is indistinguishable from a
forgotten action list: `call_llm` treats that as a bug and fires a `debug_assert!(false, ...)`,
so **every unbind panicked the connection task in a dev build**. `.with_no_actions()` states the
intent, and the assert no longer fires.

All three event response examples were `{"type": "placeholder", "event_id": "..."}`. The
response example is rendered verbatim into the prompt, so the model was being shown an action
named `placeholder` as the way to answer. They are now real responses, with a failure case as
an alternative example on each.

Search now reports `scope`, `filter` (rendered back to RFC 4515 text such as
`(&(objectClass=person)(cn=j*))`) and the requested `attributes`. None of them is enforced —
the model decides what matches — but it could not decide before, because it was only told the
base DN.

## Actions

`ldap_bind_response`, `ldap_search_response`, `ldap_add_response`, `ldap_modify_response`,
`ldap_delete_response`, `close_connection`. All carry structured fields; the BER encoding is
built here.

`wait_for_more` was removed. Messages are framed by length and reassembled by the session, and
the session only ever consumed an `ActionResult::Output` — `WaitForMore` was discarded, leaving
the client waiting for a response that would never arrive.

Result codes: 0 success, 2 protocolError, 32 noSuchObject, 49 invalidCredentials, 50
insufficientAccessRights, 53 unwillingToPerform, 68 entryAlreadyExists.

## Failure behaviour

Every operation has a default response, so a model failure never leaves the peer waiting for
its own timeout: bind defaults to invalidCredentials, search to an empty but successful result
set, and add/modify/delete to unwillingToPerform. A model that returns `close_connection` with
no response closes the connection; one that returns nothing gets the default plus a WARN.

**A model *failure* is answered differently from a model *silence*.** The defaults above all
describe an outcome the directory chose, and the search default is a **success** — resultCode 0
with no entries, which a client reads as "nothing matched". Returning one of those when the
backend is unreachable misreports an outage as a decision. So when `call_llm` returns `Err`,
`respond_via_llm` answers `unavailable` (52), or `busy` (51) when
`crate::llm::is_overload_error` says the failure was capacity exhaustion, encoded under the
response tag matching the request. Neither is ever a success. Covered by
`tests/server/ldap/llm_failure_test.rs`.

Bind success is now read structurally from the encoded BindResponse (`bind_succeeded`) rather
than by scanning the byte stream for `0x61`, which any DN or diagnostic message containing that
byte could fool.

`ldap_unbind` is raised on a detached task rather than awaited. Awaiting it delayed closing the
socket by a whole model round-trip — measured with `ldapadd` against a real model, the add was
answered promptly and the client then sat for fifteen seconds waiting for a teardown blocked on
an LLM call whose result is discarded.

## Not implemented

SASL (the mechanism is reported so it can be refused), StartTLS, LDAPS, referrals, controls,
extended operations, compare, modifyDN, abandon, schema validation, and paged results. Search
scope, filter and requested-attribute list are decoded and reported but never evaluated.

Note there are two copies of the BER *encoders* — `actions.rs` builds responses, `mod.rs`
builds defaults and errors. They agree today; if you change one, change both.

## Manual verification

```bash
./cargo-isolated.sh run --release --no-default-features --features ldap
# start on 13890 with static handlers for ldap_bind / ldap_search / ldap_add / ldap_modify /
# ldap_delete, then:

ldapsearch -x -H ldap://127.0.0.1:13890 -D "cn=admin,dc=example,dc=com" -w secret \
           -b "dc=example,dc=com" "(objectClass=person)" cn mail
ldapadd    -x -H ldap://127.0.0.1:13890 -D "cn=admin,dc=example,dc=com" -w secret -f add.ldif
ldapmodify -x -H ldap://127.0.0.1:13890 -D "cn=admin,dc=example,dc=com" -w secret -f mod.ldif
ldapdelete -x -H ldap://127.0.0.1:13890 -D "cn=admin,dc=example,dc=com" -w secret \
           "cn=testuser,dc=example,dc=com"
```

Verified 2026-08-05 against OpenLDAP's client tools: bind succeeds, the search returns both
entries with all attributes and `result: 0 Success`, and add/modify/delete all return success —
with modify's two changes decoded (`2 changes` in the log).

## Testing

`tests/server/ldap/e2e_test.rs` — 7 tests via the `ldap3` crate (bind success/failure, search,
filtered search, add, modify, delete). All pass. Note the add/modify/delete tests define no
mock for their own event and assert nothing about the result code, which is how those
operations could be entirely unimplemented while the tests stayed green.

## References

- [RFC 4511: LDAP Protocol](https://tools.ietf.org/html/rfc4511)
- [RFC 4515: LDAP Search Filter String Representation](https://tools.ietf.org/html/rfc4515)
- [ITU-T X.690: BER/CER/DER](https://www.itu.int/rec/T-REC-X.690)
