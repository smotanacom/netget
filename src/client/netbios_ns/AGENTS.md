# NetBIOS Name Service Client (RFC 1001 / RFC 1002)

The `nbtstat` equivalent, and a genuine LAN-enumeration tool. The model decides which names to
ask about and which host to ask; NetGet owns the socket and the codec.

**State**: Experimental — see "Maturity" below, which is the most important section in this
file because the evidence is asymmetric and easy to overstate.
**Privilege**: `None`, declared explicitly. Querying NBNS needs no privilege at all — only
*binding* 137 does, and that is the server's problem. This client sends from an ephemeral
source port.
**Stack**: `ETH>IP>UDP>NetBIOS-NS`.

Files: `wire.rs` (pure request encoder + response decoder), `actions.rs` (vocabulary +
executor), `mod.rs` (socket loop, transaction matching, follow-up chain).

## The codec is shared with the server, deliberately

`crate::server::netbios_ns::packet` owns first-level name encoding, padding, the 12-octet
header, the opcodes/flags/rcodes/QTYPEs, `NodeType` and MAC formatting. `wire.rs` adds only the
two directions the server has no use for: **building a query** and **parsing a response**.

This follows `bgp`, `kafka` and `websocket`, whose clients import their server halves' codecs.
A second copy of `pad_netbios_name` here would be a second place for the wildcard bug below to
come back — and it is a bug that has already cost this project a debugging pass once.

### The two encoding traps, inherited rather than re-solved

Both were live bugs on the server side until a captured `nmblookup` datagram disagreed with
what the code produced:

- **The wildcard `*` pads with NUL, not space** (RFC 1001 §17). It is `'*'` followed by fifteen
  `0x00`, encoding to `CKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`. Space padding gives
  `CKCACACACACACACACACACACACACACAAA`, which no NBNS implementation recognises. **Every node
  status query uses the wildcard**, so this is on the client's main path, not an edge case.
- **`trim_end()` does not strip NULs** — `char::is_whitespace('\0')` is false — so a name
  decoded out of a node status list would otherwise reach the model as `"*\0\0…"`.
  `split_netbios_name` trims `[' ', '\0']`, and `parse_nbstat_rdata` goes through it for every
  entry.

A third, on the response side and specific to this client: the node status name list carries
the **raw 16 octets**, not the first-level encoded 32 (RFC 1002 §4.2.18). Decoding those as
encoded is the mirror image of the classic server-side mistake, and produces a list of names
that look like garbage.

## A query is a transaction, not a stream

NBNS has no session. An answer is tied to its question by the 16-bit `NAME_TRN_ID` and by
nothing else, so `run_query` sends and then reads until the matching id arrives or the deadline
passes. There is no free-running receive loop, and that is what makes the two rules the
protocol actually requires structural instead of aspirational:

- **A reply whose transaction id does not match is discarded**, counted, and logged at **WARN**
  (not DEBUG: a well-formed answer to a question we did not ask is either a stray late reply or
  somebody trying to get a name into our cache, and it is rare enough that logging every one
  costs nothing). Three things are thrown away here and each is real: a datagram from an
  unrelated conversation, one that does not decode, and a *request* (`R=0`, which
  `wire::parse_response` refuses outright — answering one would make this client a reflector).
  **Only the first of the three is WARN**; the other two arrive as an undecodable datagram and
  are logged at DEBUG, because a socket on a link sees malformed traffic as background noise.
- **Silence is a normal answer.** A node that does not hold the queried name says nothing —
  there is no refusal to send. The deadline raises `netbios_query_timeout`, which is an
  ordinary event on the ordinary path, not an error. Its payload carries `ignored_datagrams`,
  which is the difference between "nobody is there" and "something answered and it was not for
  us".

The transaction id is `rand::random()`, not a counter: it is the only thing tying a reply to
this question, and a predictable one lets anything on the path answer for the real responder.

### Why the socket sits behind a `Mutex`

Both the LLM path and injected `[ send ]` commands run transactions on the one socket. Two
overlapping transactions would steal each other's replies, so the guard is held for the whole
send-then-receive. It covers **socket I/O only** and is bounded by `query_timeout_secs`; no LLM
call and no `AppState` access happens under it. That is the narrow, deliberate exception to the
repo's "never hold a guard across an await" rule — here serialization *is* the requirement.

## Follow-ups are bounded twice, and the second bound is the interesting one

A node status reply lists names the model will want to resolve next, so answer → question →
answer is the point of this client rather than an accident. Two independent bounds:

1. **A depth cap**, `MAX_FOLLOWUP_DEPTH = 6`.
2. **An explicit work queue instead of recursion.** `run_actions` drains a `VecDeque` of
   `(action, depth)`, so stack depth is constant however many rounds occur.

The root CLAUDE.md prescribes "box the recursive call and cap it". The queue is strictly
better and is what the DNS client converged on after a non-converging model recursed ~200 deep
and overflowed the stack, taking the whole process down (`IMPROVEMENTS.md` item 49). Carrying
the depth alongside each queued action gives the same cap with no boxed self-call at all.

The per-client LLM budget (`crate::client::llm_budget`) is the third backstop.

**Nothing here discards `result.actions`.** `report()` returns the model's actions to the
caller's queue; it never counts them, logs their length, or drops them. That is the single most
common client defect in this repository.

## What the model sees and controls

**Actions** — one list. `get_async_actions` carries the whole vocabulary, `get_sync_actions` is
empty, and every event attaches the same list. `client_llm_action_set` unions async ∪ sync ∪ the
firing event's actions, and a client has one LLM entry point, so it *cannot* express a
narrowing; duplicating the list into both methods (which ~40 clients do) only obscures that.

| Action | Effect |
|---|---|
| `send_netbios_name_query` | resolve a name — QTYPE `NB`. `name`, `suffix`, optional `target_address` / `target_port` / `broadcast` |
| `send_netbios_node_status_query` | list every name a host holds — QTYPE `NBSTAT`. `name` defaults to the wildcard `*` |
| `wait_for_more` | end the turn without asking anything |
| `disconnect` | release the socket (UDP has no wire close) |

**Events** — all five raised in `mod.rs`, all carrying `.with_actions(...)`:

| Event | Raised when |
|---|---|
| `netbios_ns_connected` | socket bound; NBNS has no handshake, so this means "ready to ask" |
| `netbios_name_response` | a positive NB answer matched the outstanding transaction id |
| `netbios_node_status_response` | a host listed its own names |
| `netbios_query_timeout` | nothing matching arrived — **normal**, not an error |
| `netbios_negative_response` | a non-zero RCODE |

### The suffix is a structured field, never text

Every name carries a separate `suffix` number plus a readable `suffix_label`.
`FILESERVER<0x00>` (workstation) and `FILESERVER<0x20>` (file server) are *different names* that
may resolve to different hosts, so folding the suffix into the name string silently merges two
questions into one. `mac_address` is a formatted `"00:11:22:33:44:55"` string and addresses are
dotted quads. **There are no raw bytes and no base64 anywhere in this vocabulary.**

`parse_suffix` accepts a number (`32`) or an explicitly-prefixed hex string (`"0x20"`), and it
is **literally the same function** the NBNS server's actions call
(`crate::server::netbios_ns::packet::parse_suffix_value`), so a suffix observed in an event can
be handed straight back and the two halves cannot drift.

A **bare** digit string is refused. That is not fussiness: the two halves used to disagree on
exactly this input — the server read `"20"` as hex (0x20, the file server) and this client read
it as decimal (20 = 0x14) — under a doc comment in `wire.rs` asserting they used the same
contract. Same JSON, two different NetBIOS names, in one session, on a protocol whose entire
hazard is answering for or asking about the wrong name. It is the `send_tcp_data` text-or-hex
ambiguity the root `CLAUDE.md` records, and it gets the same answer: the sender says which, and
neither side sniffs. `tests/client/netbios_ns/e2e_test.rs::the_two_halves_read_a_suffix_the_same_way`
pins the agreement across both spellings and both rejections.

## Startup parameters

Two, both optional, both actually read in `connect_with_llm_actions`, neither `unwrap()`ed:

- `port` — UDP port on the target. Overrides the port in `remote_addr` and supplies one when
  `remote_addr` is a bare address. Defaults to 137. This exists so a test can drive the client
  against a high port; no privilege is involved either way.
- `query_timeout_secs` — deadline for a matching reply before `netbios_query_timeout`.
  Defaults to 3. Zero is rejected at startup: a zero deadline discards every reply before it
  can arrive.

A literal address never reaches the system resolver. `getaddrinfo("127.0.0.1")` is a real call
measured at 8.25s under concurrency on macOS and can only ever return what was already written
down; `resolve_target` parses `SocketAddr` then `IpAddr` first and only calls `lookup_host` for
an actual hostname.

## Maturity: the evidence is asymmetric, and that is the whole rating

**The encode direction has genuinely independent evidence.** `SAMBA_NAME_QUERY` and
`SAMBA_NODE_STATUS_QUERY` in `tests/client/netbios_ns/e2e_test.rs` were captured off loopback
with `tcpdump` while Samba 4.24.6's `nmblookup` sent them, and this client's encoder reproduces
both **byte for byte**, transaction id included. NetGet wrote none of those bytes. The queries
this client puts on the wire are indistinguishable from a real Samba client's.

That capture was taken independently for this suite and matches, after the transaction id, the
separately captured literals in `tests/server/netbios_ns/e2e_test.rs` — two runs of a
third-party client, not one transcription that could have been mis-copied.

**The decode direction has none.** The only NBNS responder this client has ever been driven
against is NetGet's own server (plus a raw stand-in built from the same codec). That is
same-project evidence: it shows the two halves agree, not that either matches RFC 1002.

So Beta's claim — "works against real clients" — is unsupported in the direction that matters,
and the rating is **Experimental**. This is deliberately not the `wireguard` mistake recorded in
the root CLAUDE.md, where a demotion for missing evidence stepped down one notch by reflex into
a rating the same evidence also ruled out.

Why no third-party responder is reachable, verified rather than assumed on the server side:
`nmblookup` offers no port option (`-U` takes an address only), `--option="nbt port=…"` is
accepted by the config parser and ignored by the client, and binding UDP 137 needs root on
macOS. An `#[ignore]`d root test is not evidence.

**What would move it to Beta**: any NBNS responder that can be reached on a high port, or a
privileged CI lane running a real one. Byte-identical queries are necessary and not sufficient.

## Not implemented

- **Retransmission.** One datagram per action. Real clients retry (`nmblookup` sends two); the
  model can simply issue the action again, and hiding a retry inside the action would make
  `ignored_datagrams` and the timeout misleading.
- **WACK, redirects, name registration, refresh and release** (RFC 1002 §4.2.9–§4.2.12,
  §4.2.16). This client only asks questions.
- **NetBIOS scopes** beyond what the codec echoes.
- **The datagram service (UDP 138) and session service (TCP 139)** are different protocols.
- **IPv6**: NBNS is IPv4-only by construction — its resource records carry four-octet
  addresses. `resolve_target` refuses a hostname that resolves to no IPv4 address, but a
  *literal* IPv6 `remote_addr` short-circuits the lookup and is returned unchecked, while the
  socket is unconditionally bound to `0.0.0.0`. So an IPv6 literal is accepted at startup and
  fails later on `send_to` rather than being refused up front.

## Example prompts

```
Connect to 192.168.1.10:137 via NetBIOS-NS and run a node status query to list every name
that host holds, then report the workgroup and the adapter MAC.
```

```json
{"type": "open_client", "protocol": "NetBIOS-NS", "remote_addr": "192.168.1.10:137",
 "instruction": "Node status the host, then resolve every unique name it reported at suffix 0x20.",
 "startup_params": {"query_timeout_secs": 2}}
```
