# NetBIOS Name Service Server (RFC 1001 / RFC 1002)

UDP name service. The model decides which NetBIOS names exist, what they resolve to, and what
a node status listing says. NetGet owns the socket and the codec.

**State**: Experimental — see "Maturity" below, which explains precisely what is and is not
evidence here. **Privilege**: declares `PrivilegedPort(137)`; the preflight fires only when the
requested port is actually below 1024, so a test on 13137 needs no privileges.
**Stack**: `ETH>IP>UDP>NetBIOS-NS`. **Connectionless**: declared, so the 10-second idle sweep
reaps the per-remote-address bookkeeping entries that nothing else would ever close.

Files: `packet.rs` (pure codec, no I/O), `actions.rs` (LLM vocabulary + executor),
`mod.rs` (socket loop + the silence rule).

## The single most important property: it goes silent, it never guesses

NBNS is a **caching** name service. A querier that gets a POSITIVE NAME QUERY RESPONSE stores
the address for the TTL and uses it for every later connection to that name. A fabricated
answer therefore does not fail once — it redirects that host's traffic for hours, and keeps
doing so after the backend recovers. This is the `mdns` hazard, unicast and worse: mDNS
poisons a link, NBNS poisons whichever host asked.

So this server has **no default reply anywhere**. Nothing in `actions.rs` can synthesise a
response; every byte that reaches the wire came out of an action the model named. When no
usable answer is produced, nothing is sent, and the querier falls back exactly as it would if
no NBNS server were listening — which on a broadcast query is the normal behaviour of every
node that does not hold the name.

That puts NBNS in the deliberately-silent class in the root `CLAUDE.md`, and it is a strong
member of it: every response the protocol defines is a positive assertion about a name, and
even the "negative" form (RFC 1002 §4.2.14) asserts that the name *does not* exist — which a
querier may also cache.

### The wire cannot carry the distinction, so the log must

Because every fail-closed path is byte-for-byte identical to a dead server, the log is the only
place a refusal, a deliberate silence and an outage can be told apart. `RadiusServer::decide`
is the model for this; `NetbiosNsServer::decide` is the same discipline applied where the wire
affords nothing:

| Situation | Wire result | Logged decision |
|---|---|---|
| Model returns `send_netbios_name_response` | positive NB answer | `decision=model_answer` |
| Model returns `send_netbios_node_status_response` | NBSTAT answer | `decision=model_answer` |
| Model returns `send_netbios_negative_response` | NULL RR + non-zero RCODE | `decision=model_reject` |
| Model returns `no_response` | **nothing** | `decision=model_silent` |
| Model returns no actions at all | **nothing** | `decision=fail_closed_no_action` |
| Model's action fails to encode | **nothing** | `decision=fail_closed_action_error` |
| LLM call errors / times out | **nothing** | `decision=fail_closed_llm_error`, plus `category=overloaded`/`category=unavailable` from `WireFailure::classify` |

Every `fail_closed_*` line is logged at ERROR, because on this protocol it is invisible from
outside. `decision=model_silent` is logged at INFO: it is a real answer, not a failure, and
conflating the two is the OAuth2 defect in the direction that matters here.

The error text never reaches the wire because nothing reaches the wire. There is no
`WireFailure` string to leak.

## First-level name encoding — the thing implementations get wrong

A NetBIOS name is **always 16 octets**: 15 octets of name, space-padded, then one **suffix**
octet selecting the service. Those 16 octets expand to 32 by splitting each into two nibbles,
high first, and adding `'A'` — so every wire character is in `A..=P`.

```
'*'  = 0x2A -> nibbles 2, 10 -> 'C', 'K'
0x00        -> nibbles 0, 0  -> 'A', 'A'
0x20 (' ')  -> nibbles 2, 0  -> 'C', 'A'
```

Two traps, both of which were live bugs here until a captured `nmblookup` datagram disagreed
with what the code produced:

- **The wildcard `*` pads with NUL, not space.** RFC 1001 §17 defines it as `'*'` followed by
  fifteen `0x00`, encoding to `CKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`. Space padding gives
  `CKCACACACACACACACACACACACACACAAA`, which no NBNS implementation recognises.
- **`trim_end()` does not remove NULs** — `char::is_whitespace('\0')` is false — so the
  wildcard reached the model as `"*\0\0…"`. `split_netbios_name` trims `[' ', '\0']`.

A third: the header is **12** octets (six 16-bit fields), not 16. Counting the fields wrong
shifts every subsequent offset, and the symptom is a name that decodes to garbage rather than
an obvious error.

A fourth, in the other direction: the node status name list carries the **raw 16 octets**, not
the encoded 32 (RFC 1002 §4.2.18). Encoding them there makes every real client render
gibberish.

## What the model sees and controls

**Events** — all three raised in `mod.rs`, all carrying `.with_actions(...)`:

| Event | Raised when | Data |
|---|---|---|
| `netbios_name_query` | OPCODE 0, QTYPE `NB` | `name`, `suffix`, `question_type`, `source_address`, `transaction_id` |
| `netbios_node_status_request` | OPCODE 0, QTYPE `NBSTAT` | `name`, `suffix`, `source_address`, `transaction_id` |
| `netbios_name_registration` | OPCODE 5 | `name`, `suffix`, `address`, `group`, `source_address`, `transaction_id` |

Any other opcode — RELEASE, REFRESH, WACK — is dropped with a DEBUG line. Answering an opcode
with no event would mean answering without asking the model, which is the thing this protocol
must never do. A datagram whose `R` bit is set is refused by the decoder: answering a response
turns the server into a reflector.

**Actions**

| Action | Effect |
|---|---|
| `send_netbios_name_response` | positive NB answer: `name`, `suffix`, `addresses[]`, optional `ttl`, `group`, `node_type` |
| `send_netbios_node_status_response` | `names: [{name, suffix, group, active}]` + `mac_address` |
| `send_netbios_negative_response` | `rcode` by name (`name_not_found`, `name_active`, …) or number |
| `no_response` | explicit silence — a real answer, distinct from a timeout |

No async actions: NBNS is purely reactive, and a user-triggered action would have no querier
to send its datagram to.

### The suffix is a structured field, never text

`suffix` is a number (or a hex string like `"0x20"`), never part of the `name` string. The
model cannot be expected to hand-encode a control octet into text, and `FILESERVER<0x00>` and
`FILESERVER<0x20>` are *different names*. Same rule for `mac_address`, which is a formatted
string (`"00:11:22:33:44:55"`) parsed by the executor, and for addresses, which are dotted
quads. There are no raw bytes and no base64 anywhere in this vocabulary.

### Per-request protocol instances

The registry's `NetbiosNsProtocol` is context-free and **cannot build a response**;
`execute_action` on it returns an error saying so (`no_response` is the exception — saying
nothing needs nothing from the request, and must be trivially reachable). `mod.rs` builds one
`NetbiosNsProtocol::for_request(...)` per datagram carrying that request's `NAME_TRN_ID`, its
OPCODE, its RD bit and the exact question NAME field.

**Those four are echoed by the server, never chosen by the model** — deliberately, and it is a
departure from how `dns` does it (where `query_id` is an action parameter). Three reasons:

1. A response with the wrong transaction id is discarded, which is indistinguishable from an
   outage; a response with the wrong *name* is cached against a host nobody asked about.
2. A registration response differs from a query response **only** in the OPCODE, so having the
   server supply it removes an entire action parameter the model would otherwise have to get
   right, and removes the possibility of answering a query with a registration response.
3. Echoing the question's NAME field byte-for-byte preserves any NetBIOS scope the sender used
   without this code having to re-encode it.

This is the `radius`/`ntp` precedent — identifier and authenticator carried in the request
context — not a deviation from repo practice.

## Startup parameters

Two, both optional and both actually read in `spawn_with_llm_actions` (a declared knob that
turns nothing is the `startup_param_drift_test` defect):

- `default_ttl` — seconds a positive answer may be cached when the action gives no `ttl`.
  Defaults to 10800.
- `node_type` — `b`/`p`/`m`/`h`, the ONT bits used when the action gives no `node_type`.
  Defaults to `b`.

Both are propagated with `?`; neither is `unwrap()`ed. A bad value fails startup with a message
naming the accepted set.

## Not implemented

- **WINS**: no name table, no registration state, no conflict detection, no name refresh or
  release handling. There is nothing to conflict *with* — protocols must not implement storage,
  so the model invents every name on every request. A registration is a question put to the
  model, not a record written anywhere.
- **Nothing about the real machine is consulted.** No local hostname, no interface enumeration,
  no share list. The MAC in a node status response is whatever the model said.
- **Secondary-level encoding / scope** is decoded and echoed but not otherwise interpreted;
  scope-aware name matching is the model's business.
- **NAME RELEASE, NAME REFRESH, WACK, redirects** (RFC 1002 §4.2.9–§4.2.12, §4.2.16): dropped.
- **Name compression pointers** in a question: refused, not followed.
- **Datagram service (UDP 138) and session service (TCP 139)** are different protocols and are
  not here.
- **IPv6**: NBNS is IPv4-only by construction; the RR carries four-octet addresses.

## Maturity: why Experimental and not Beta

Samba's `nmblookup` is a genuine third-party client, and the request literals in
`tests/server/netbios_ns/e2e_test.rs` were **captured off the wire while it ran** (Samba
4.24.6, `tcpdump -i lo0`), so the decoding direction is pinned by bytes NetGet did not write.

But `nmblookup` cannot be pointed at another port. Verified rather than assumed:

- `nmblookup --help` and `nmblookup(1)` offer no port option. `-U` takes an address only;
  `-U 127.0.0.1:13137` fails to parse the target.
- `--option="nbt port=13137"` is *accepted* by the smb.conf parser — it is a real Samba
  parameter for the source4 NBT **server** — and is ignored by this client. Confirmed by
  packet capture: with that option set, the query still went to `127.0.0.1.137`, and a socket
  bound to 13137 received nothing.
- Binding UDP 137 requires root on macOS (`EACCES` as an ordinary user).

So driving `nmblookup` against a NetGet server needs a privileged run, and CLAUDE.md is
explicit that an `#[ignore]`d root test is not evidence. **The response direction has therefore
never been validated by an independent implementation**, which is exactly the "works against
real clients" claim Beta makes. Experimental is the honest rating, and `metadata().notes` says
so in as many words.

What would move it to Beta: a privileged CI lane, or any NBNS client that accepts a port.
`nmblookup` is not it.

## Example prompts

```json
{"type": "open_server", "port": 137, "base_stack": "netbios_ns",
 "event_handlers": [{"event_pattern": "netbios_name_query", "handler": {"type": "script",
   "language": "python",
   "code": "name = event.get('name', '')\nif name == 'FILESERVER':\n    respond([{'type': 'send_netbios_name_response', 'name': name, 'suffix': event.get('suffix', 0), 'addresses': ['192.168.1.10'], 'ttl': 3600, 'group': False}])\nelse:\n    respond([{'type': 'send_netbios_negative_response', 'rcode': 'name_not_found'}])"}}]}
```

```
NetBIOS name server on port 137. FILESERVER<0x20> is 192.168.1.10, WORKGROUP<0x00> is a group
name held by 192.168.1.10 and 192.168.1.11. Say name_not_found for anything else.
```

```
listen on netbios_ns port 137 and answer node status requests with the names NETGETHOST<0x00>
and WORKGROUP<0x00>, adapter 02:00:5e:10:00:01.
```
