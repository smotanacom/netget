# DNS Protocol Implementation

## Overview

DNS (Domain Name System) server implementing RFC 1035 standard for domain name resolution. The LLM can respond to
queries with A, AAAA, CNAME, MX, TXT records, and NXDOMAIN responses using structured actions.

**Status**: **Stable**, set 16 September 2026. See "Maturity: the six conditions"
at the foot of this file for what was checked and what the rating does *not*
cover.
**RFC**: RFC 1035 (Domain Names), RFC 3596 (AAAA), RFC 1034 (Concepts)
**Port**: 53 (UDP), declared as `PrivilegeRequirement::PrivilegedPort(53)`

### What "Stable" covers here

Answers built by the structured actions are RFC 1035 responses that a real stub
resolver accepts: correct wire format, the client's transaction ID echoed, and
the question section repeated. Every record type the model can produce — A, AAAA,
CNAME, MX, TXT — plus NXDOMAIN is decoded by **two** independent resolvers, and
one query is run the way a person actually runs `dig`, with EDNS offered and no
flags.

Not covered, because not implemented: TCP transport, EDNS0, DNSSEC, multi-record
answers, and truncation with the TC bit (see Limitations). This section used to
say record types "outside A/AAAA/CNAME/MX/TXT" were uncovered, which read as
though those five were — three of them were not decoded by anything independent
until this pass.

E2E coverage lives in `tests/server/dns/`: `test.rs` (note: `test.rs`, not
`e2e_test.rs` as most protocols use), `dig_test.rs`, `kdig_test.rs`,
`llm_failure_test.rs` and `bounds_test.rs` — **13 tests, all passing**. This
line said "six" and listed three files, which was correct before `kdig_test.rs`
and `bounds_test.rs` existed and is the sort of count that goes stale the moment
a file is added; derive it with `cargo test --features dns --test server -- dns`
rather than reading it here. (Earlier still, this file claimed the four in
`test.rs` "currently fail before they reach the DNS layer" over a
`DocumentationRequired` retry, which was already stale when it was written.)

**`dig_test.rs` and `kdig_test.rs` are what make the Stable rating mean something.**
`test.rs` drives the server with hickory-client while the server encodes with
hickory-proto - the same project, the same codec - so on its own it proves the
wire format self-consistent rather than correct. That is the circularity that
kept `rss` at Experimental until `feed-rs` did its parsing. ISC BIND's `dig` and
Knot DNS's `kdig` share no code with hickory or with each other, each checks that
the transaction id it chose comes back and that the question matches what it
asked, and each **fails rather than skipping** when its binary is absent (the
`npm` precedent). See "Two resolvers, and why the second one exists" below.

## Library Choices

- **hickory-proto** (formerly trust-dns) - DNS protocol parsing and construction
    - Used for parsing incoming DNS queries
    - Used for constructing DNS response messages
    - Handles binary DNS wire format automatically
    - Provides high-level Record, RData, Message types

**Rationale**: hickory-proto is the de facto standard for DNS in Rust. It provides robust parsing/serialization and
eliminates the need for manual binary protocol handling. The LLM doesn't need to understand DNS binary format - it just
provides semantic data (domain, IP, TTL) and the library handles encoding.

## Architecture Decisions

### 1. Action-Based LLM Control

The LLM doesn't manipulate raw DNS packets. Instead, it returns semantic actions like:

- `send_dns_a_response` - Return IPv4 address for domain
- `send_dns_aaaa_response` - Return IPv6 address for domain
- `send_dns_mx_response` - Return mail exchange record
- `send_dns_txt_response` - Return text record
- `send_dns_cname_response` - Return canonical name alias
- `send_dns_nxdomain` - Domain does not exist
- `send_dns_response` - Send raw hex packet (advanced)
- `ignore_query` - No response

Each action includes required fields (query_id, domain, record data) and optional fields (TTL).

### 2. Stateless Request-Response

DNS is connectionless UDP protocol:

- Each query is independent
- No connection state maintained
- "Connection" in UI represents recent queries from same client
- Query ID from client packet must be echoed in response

### 2b. Responses Echo the Question Section

Every response built by the structured actions repeats the question it answers
(RFC 1035 §4.1.2). This is not cosmetic: real stub resolvers - glibc's,
systemd-resolved, `dig` - compare the response's question section against the
query they sent and discard responses that do not match. The question is
reconstructed in `new_response()` (`actions.rs`) from the action's `domain` plus
the record type implied by the action, so `send_dns_a_response` produces a
`domain A IN` question. `send_dns_nxdomain` and `send_dns_cname_response` accept
an optional `query_type` because the type cannot be inferred from the answer in
those two cases.

Consequence for the two ID-bearing fields the client uses to correlate a
response:

- `query_id` - taken verbatim from the action. Out-of-range values are rejected
  rather than truncated with `as u16`, because a truncated ID produces a
  response the client silently drops.
- question section - as above.

Both are why a **static** event handler cannot serve DNS answers: static
handlers emit fixed action JSON with no access to the event, so they cannot echo
either the client's random transaction ID or the queried name. Use **script**
mode for deterministic DNS; static mode is only useful here for `ignore_query`
(a DNS blackhole).

### 3. hickory-proto Integration

Parsing flow:

1. Receive UDP datagram
2. Parse with `DnsMessage::from_vec()`
3. Extract query ID, domain name, query type, query class
4. Send to LLM as `dns_query` event
5. LLM returns action with semantic data
6. Action executor builds `DnsMessage` response
7. Serialize with `message.to_vec()`
8. Send UDP datagram back to client

### 4. Dual Logging

- **DEBUG**: Query summary ("DNS query: example.com A IN")
- **TRACE**: Full hex dump of DNS packets (both request and response)
- Both go to netget.log and TUI Status panel

### 5. Connection Tracking

Each DNS query creates a "connection" entry in ServerInstance:

- Connection ID: Unique per query
- Protocol info: `ProtocolConnectionInfo::empty()` - there is no DNS-specific
  variant, so no per-query domain list is surfaced to the UI
- Tracks: bytes received/sent, packets received/sent (sent counters are updated
  via `AppState::update_connection_stats` after the response goes out)
- Status: Immediately active, no persistent state

## LLM Integration

### Event Type

**`dns_query`** - Triggered when DNS client sends a query

Event parameters:

- `query_id` (number) - DNS transaction ID from request packet
- `domain` (string) - Domain name being queried
- `query_type` (string) - Record type (A, AAAA, MX, TXT, CNAME, etc.)

### Available Actions

#### `send_dns_a_response`

Return IPv4 address (A record).

Parameters:

- `query_id` (required) - Echo from request
- `domain` (required) - Domain name
- `ip` (required) - IPv4 address string (e.g., "192.0.2.1")
- `ttl` (optional) - Time-to-live in seconds (default: 300)

#### `send_dns_aaaa_response`

Return IPv6 address (AAAA record).

Parameters:

- `query_id` (required)
- `domain` (required)
- `ip` (required) - IPv6 address string (e.g., "2001:db8::1")
- `ttl` (optional, default: 300)

#### `send_dns_mx_response`

Return mail exchange record.

Parameters:

- `query_id` (required)
- `domain` (required)
- `exchange` (required) - Mail server domain (e.g., "mail.example.com")
- `preference` (optional) - Priority, lower = higher priority (default: 10)
- `ttl` (optional, default: 300)

#### `send_dns_txt_response`

Return text record.

Parameters:

- `query_id` (required)
- `domain` (required)
- `text` (required) - Text data to return, **at most 255 octets**: it is emitted
  as a single DNS `<character-string>`, whose length is one octet (RFC 1035
  §3.3). Longer text is refused with a message naming the limit, rather than
  split across strings or left to fail inside hickory's encoder — see Bounds
- `ttl` (optional, default: 300)

#### `send_dns_cname_response`

Return canonical name (alias).

Parameters:

- `query_id` (required)
- `domain` (required)
- `target` (required) - Target domain name
- `ttl` (optional, default: 300)

#### `send_dns_nxdomain`

Domain does not exist.

Parameters:

- `query_id` (required)
- `domain` (required)
- `query_type` (optional, default: `A`) - echoed in the question section so the
  client can match the (answer-less) response to its query

#### `send_dns_response` (Escape hatch)

Send a complete, hand-assembled DNS response message, hex-encoded. Intended only
for record types with no dedicated action (NS, SOA, PTR, SRV, CAA, ...).

- `data` must be valid hex and at least 12 bytes (a DNS header). Invalid hex is
  rejected with an error. There is no plain-text fallback: the earlier behaviour
  of sending the raw string bytes when hex decoding failed put non-DNS garbage
  on the wire and left clients timing out with no diagnostic.
- The caller is responsible for the transaction ID and the question section;
  nothing is filled in.

Note: this is the one DNS action that takes wire bytes as a parameter, which
runs against the project rule that action parameters carry structured data
rather than encoded bytes. It is kept because dropping it would leave the
unsupported record types unreachable, and its description steers the model to
the structured actions first.

#### `ignore_query`

Don't send any response to this query.

### Example LLM Response

```json
{
  "actions": [
    {
      "type": "send_dns_a_response",
      "query_id": 12345,
      "domain": "example.com",
      "ip": "93.184.216.34",
      "ttl": 300
    },
    {
      "type": "show_message",
      "message": "Resolved example.com to 93.184.216.34"
    }
  ]
}
```

## Connection Management

### Connection Lifecycle

1. **Query Received**: UDP datagram arrives on port 53
2. **Register**: New ConnectionId created for this query
3. **Track**: Added to ServerInstance.connections with:
    - `ProtocolConnectionInfo::empty()`
    - bytes_received, packets_received = 1
4. **Process**: Parse query, dispatch through `call_llm` (which first tries any
   configured script/static event handler and only falls back to a model call if
   none matches), execute action
5. **Respond**: Send UDP response
6. **Update**: Track bytes_sent, packets_sent
7. **Persist**: Connection remains in UI to show recent activity

Note: DNS has no persistent connections. Each query-response is independent.

## Bounds

Every number this server enforces, and where. `tests/server/dns/bounds_test.rs`
drives each one and each doc comment there records what the failure looked like
with the bound removed — a bound nobody tested is a comment.

| bound | value | where | how it is driven |
|---|---|---|---|
| receive buffer | 4096 | `mod.rs` | a socket: an 8 KiB query is dropped and never becomes a prompt |
| `query_id` | 0-65535 | `parse_query_id` | the executor; refused, never narrowed with `as u16` |
| MX `preference` | 0-65535 | `execute_send_dns_mx_response` | the executor |
| raw message floor | 12 octets | `execute_send_dns_response` | the executor |
| TXT character-string | 255 octets | `MAX_CHARACTER_STRING_LEN` | the executor |

**DNS declares no `max_inbound_bytes`**, and that is deliberate rather than an
omission: it is a "fixed-size read" entry in
`tests/max_inbound_bytes_declaration_test.rs`'s baseline, on the reason "single
`recv_from` into a fixed 4096-byte buffer; no TCP path exists". The allocation is
a compile-time constant, so a declaration would be a knob that does nothing.

What the buffer guarantees is weaker than a refusal and is worth stating
plainly: `recv_from` discards whatever does not fit, so an over-large datagram is
**truncated**, then fails to parse as DNS, then is dropped with a WARN. No
allocation and no prompt scales with what the peer sent, which is the property
`max_inbound_bytes` exists for; the peer gets no diagnostic, which CoAP's 4.13
does give. There is nothing honest to echo — the truncated bytes may not even
contain a complete question section.

## Known Limitations

### 1. UDP Only

- No TCP support (RFC 1035 specifies TCP for large responses), so a client that
  retries over TCP after a truncated answer gets nothing
- The receive buffer is 4096 bytes, which accommodates EDNS0-sized queries
- EDNS0 itself is not implemented: the OPT record in a query is ignored and no
  OPT record is added to responses
- Oversized responses are not truncated with the TC bit set; they are simply sent

### 2. Single Answer Per Response

- Current action design returns one record per response
- No support for multiple A records in single response
- Workaround: Use `send_dns_response` with custom hex packet

### 3. No DNSSEC

- No cryptographic signatures
- No RRSIG, DNSKEY, DS, NSEC records
- Pure RFC 1035 implementation

### 4. Limited Record Types

Actions support: A, AAAA, CNAME, MX, TXT, NXDOMAIN
Missing: NS, SOA, PTR, SRV, NAPTR, CAA, and others
Workaround: Use `send_dns_response` for unsupported types

### 5. No Zone File Support

- No authoritative zone data storage
- LLM generates responses on-demand
- No persistent DNS database

### 6. No Recursive Resolution

- Acts only as authoritative server
- Doesn't forward queries to upstream resolvers
- Doesn't perform recursive lookups

## Example Prompts

### Simple A Record Server

```
listen on port 53 via dns
Respond to all A record queries for example.com with IP 93.184.216.34
For all other domains, return NXDOMAIN
```

### Multi-Record Server

```
listen on port 53 via dns
For example.com:
  - A record: 93.184.216.34
  - AAAA record: 2001:db8::1
  - MX record: mail.example.com with priority 10
  - TXT record: v=spf1 mx ~all
For mail.example.com:
  - A record: 93.184.216.35
For unknown domains, return NXDOMAIN
```

### Wildcard DNS

```
listen on port 53 via dns
For any subdomain of example.com, return CNAME pointing to www.example.com
For www.example.com, return A record 93.184.216.34
```

### Custom TTL

```
listen on port 53 via dns
Respond to A queries for example.com with 93.184.216.34 and TTL of 3600 seconds
```

## Performance Characteristics

### Latency

- **With Scripting**: Sub-millisecond response (script handles query directly)
- **Without Scripting**: 2-5 seconds (one LLM call per query)
- hickory-proto parsing: ~10-50 microseconds
- hickory-proto serialization: ~10-50 microseconds

### Throughput

- **With Scripting**: Thousands of queries per second (CPU-bound)
- **Without Scripting**: Limited by LLM response time (~0.2-0.5 QPS)
- Concurrent queries processed in parallel (separate tokio tasks)
- Concurrency is bounded by `--llm-max-concurrent` / `--llm-queue-timeout` /
  `--llm-max-queued`. This line used to read "Ollama lock serializes LLM API
  calls"; there is no such lock. `--ollama-lock` is parsed and read by nothing,
  its six plumbing hops were deleted, and `tests/ollama_lock_is_a_noop_test.rs`
  fails if anything starts reading it again. Do not reason about DNS concurrency
  from it.

### Scripting Compatibility

DNS protocol is excellent candidate for scripting:

- Repetitive request/response pattern
- Deterministic responses based on domain/query type
- No complex state machine
- High query volume typical use case

What that looks like in practice:

- One LLM call at startup, in which the model writes a `script` `event_handlers`
  entry for `dns_query` (see `get_startup_examples()`, which offers exactly that
  script)
- Every subsequent query is answered in-process by the script, with **zero** LLM
  calls, because `try_execute_event_handler` runs before the model is reached
- Dramatically improves throughput

This is the model choosing to write a handler, not something NetGet does on its
own. This section used to read "Server startup generates Python/JavaScript
script (1 LLM call)" as though it were automatic; `--no-scripts` /
`ScriptingMode::Off` only takes the option away, it does not mean the option is
otherwise taken.

## References

- [RFC 1034: Domain Names - Concepts and Facilities](https://datatracker.ietf.org/doc/html/rfc1034)
- [RFC 1035: Domain Names - Implementation and Specification](https://datatracker.ietf.org/doc/html/rfc1035)
- [RFC 3596: DNS Extensions to Support IPv6 (AAAA)](https://datatracker.ietf.org/doc/html/rfc3596)
- [hickory-dns Documentation](https://docs.rs/hickory-proto/latest/hickory_proto/)
- [DNS Query Types (IANA)](https://www.iana.org/assignments/dns-parameters/dns-parameters.xhtml#dns-parameters-4)

## Failure behaviour: SERVFAIL, never silence

A query that produces no answer is answered with **SERVFAIL (RCODE 2)** rather than dropped.
`actions::build_servfail` copies the transaction ID and the question section off the request and
clears AA; a stub resolver that gets this stops waiting and moves to the next nameserver, where
silence costs it a full per-server timeout (5s in glibc) first.

**Three paths reach it, and until September 2026 only two did.** This section said "when
`call_llm` returns `Err` — backend down, overloaded, **or no usable response**". The third
clause was false: `call_llm` returns **`Ok`** whenever the model produced a syntactically valid
answer, and `protocol_results` is empty whenever that answer was made only of *common* actions
(`show_message`) or of a DNS action `execute_action` refused — an invalid IP, an out-of-range
`query_id`, a TXT string over 255 octets. The result loop then ended having written nothing and
the `Err` arm was never reached, so the one case the sentence claimed to cover was the one that
went silent. `send_servfail` is now the single exit for all three:

| `decision=` | cause | level |
|---|---|---|
| `model_answer` | an answer was written | DEBUG |
| `model_silent` | `ignore_query` — the model chose a black hole, a real decision | DEBUG |
| `fail_closed_no_action` | the model answered with nothing this server could send | **ERROR** |
| `fail_closed_llm_error` | the backend failed | WARN |
| `fail_closed_llm_overload` | the backend was overloaded (`is_overload_error`) | WARN |

`model_silent` and `fail_closed_no_action` are the pair that has to stay distinct: both begin
as "no bytes were produced", and only the first is something the operator asked for.
`tests/server/dns/llm_failure_test.rs::test_dns_distinguishes_no_usable_action_from_a_deliberate_black_hole`
drives both against one server for that reason — a fix that answered SERVFAIL to everything
would pass half of it.

The ID and question echo are not optional decoration: a resolver discards a response that fails
either check, which turns the SERVFAIL back into silence. See §2b above — this is the same
requirement the successful actions have, and it regressed once before (`6a384617`).

Covered by `tests/server/dns/llm_failure_test.rs`, which asserts the RCODE nibble on the raw
bytes as well as through a decoder, and runs the pcap oracle over both replies.

## Two resolvers, and why the second one exists

`DevelopmentState::Stable` rests on **both**, and neither is `#[ignore]`d or skip-gated — each
fails naming its package when the binary is absent:

| test | resolver | project |
|---|---|---|
| `dig_test.rs` | `dig` | ISC BIND |
| `kdig_test.rs` | `kdig` | Knot DNS, CZ.NIC |

`test.rs` covers the same ground with hickory-client, which is useful and **circular on its
own**: hickory-client decodes with the same codec hickory-proto encoded with, so it proves the
wire format is self-consistent rather than correct.

`dig` already fixed that. The second resolver is here because **one client can agree with one
bug** — in September 2026 `etcd` and `grpc` each turned out to be doing exactly that, and in
both cases no conformant implementation could complete a successful call while every existing
test passed. Two independent resolvers agreeing with each other and with us is the strongest
evidence short of the spec; two that disagree is a finding.

Between them they decode **every record type this server can produce** — A, AAAA, CNAME, MX,
TXT and NXDOMAIN — each through that resolver's own presentation writer. Until September 2026
only A and TXT were covered, which left three actions the model is offered with nothing
independent having decoded them; `metadata()` said so and it was read as a note rather than as
a gap.

Nearly every query is run with `+noedns`, honestly rather than conveniently: this server does
not implement EDNS0, and a resolver that offers EDNS and gets a reply with no OPT record may
fall back and re-query — which would be a second `dns_query` event and would break
`expect_calls`. **One query is not**: `dig_test.rs` runs the A lookup a second time with no
flags at all, because "works against real clients" has to mean the way a person invokes the
client. RFC 6891 §6.1.1 is why it works — a server that does not understand EDNS answers with
no OPT record and the requestor treats it as non-EDNS — and `expect_at_least` on that rule is
what makes a fallback re-query harmless.

Verified by answering with a fixed transaction id instead of the client's: kdig discards the
reply and the test fails. That is the class of defect a round-trip through our own codec cannot
see, because our decoder does not care what id it reads.

**Unproven:** EDNS0, TCP transport, DNSSEC, zone transfers, and record types beyond A and TXT.

## Maturity: the six conditions

(This heading read "…, and which one is missing" above a table whose every row says **yes**. It
was left over from the pass that began before condition 4 was satisfied; a reader skimming
headings would have taken the opposite of what the section says.)

The root `CLAUDE.md` defines `Stable` as six conditions. Re-derived against source on
16 September 2026 rather than inherited:

| # | condition | holds? |
|---|---|---|
| 1 | two independent third-party clients, no skip, no `#[ignore]` | **yes** — `dig` (ISC BIND) and `kdig` (Knot DNS, CZ.NIC), each hard-failing by name; between them they decode every record type this server can produce, and `dig` additionally runs one query with default flags |
| 2 | the pcap oracle is green over its wire traffic | **yes** — `llm_failure_test.rs` over both SERVFAIL paths, `bounds_test.rs` over a NOERROR answer with rdata |
| 3 | a fuzz target exists and has run clean, with a corpus | **yes** — `fuzz/fuzz_targets/dns_message.rs`; 2,777,883 runs in 91s clean, corpus includes `compression_pointer_loop` |
| 4 | every declared bound has a test | **yes, as of this pass** — see Bounds above, each verified by removal |
| 5 | both `CLAUDE.md` files verified against source in this pass | **yes** — this file and `tests/server/dns/CLAUDE.md` |
| 6 | no `#[ignore]`, no skip-when-missing gate | **yes** — `grep -rn '#\[ignore\]' tests/server/dns/` is empty |

**Two things about condition 1 were checked in this pass rather than assumed, because on the
face of it they were the reasons to withhold the rating.**

*Only A and TXT had independent evidence.* `metadata()` said so — "UNPROVEN: … record types
beyond A and TXT" — which left `send_dns_aaaa_response`, `send_dns_mx_response` and
`send_dns_cname_response` as actions the model is offered with nothing independent ever having
decoded them. A 16-octet address, a `u16` followed by a domain name, and a bare domain name are
three different ways to get an encoder wrong, and an MX preference is the only integer rdata
field in the whole action set. Both resolvers now read all three; the preference is 4660
(0x1234), so a byte-swap reads 13330 rather than something plausible.

*Every query was run with `+noedns`.* The reason given was honest but narrow — EDNS0 is not
implemented, and a resolver that offers EDNS and gets a reply with no OPT record *may* fall back
and re-query, which breaks `expect_calls(1)`. It left unasked the question that actually decides
the rating: does a resolver, invoked the way a person invokes it, get an answer at all? Every
modern resolver sends EDNS by default, so if the answer were no then "works against real
clients" would be a claim about a configuration nobody uses — the `mysql_native_password`
situation, where the rating rested on the one client permissive enough to tolerate what ships.
It is not: RFC 6891 §6.1.1 says a server that does not understand EDNS answers without an OPT
record and the requestor treats the response as coming from a non-EDNS server, and
`dig_test.rs` now runs one query with no flags to prove it. The A rule's expectation is
`expect_at_least` so a fallback re-query is harmless rather than a failure.

### What Stable does *not* mean here

It means the evidence for the implemented surface is complete. The surface is plain UDP DNS
with one record per answer: **no EDNS0, no TCP, no DNSSEC, no zone transfers, no multi-record
answers, no TC-bit truncation** — an oversize response is sent rather than truncated. Those are
whole features, each listed above and in Limitations, and nothing here says a client that needs
one of them is served.

The `openvpn` precedent is the one to check this against, and it does not apply: openvpn stays
Experimental because it implements only the front of the protocol, so no client can use it for
what the protocol is for. A resolver can use this for what DNS is for — two of them did, over
every record type it offers, one of them with default flags.

**Condition 3 is the one to re-check before trusting this table.** Nothing in CI builds
`fuzz/` — it is deliberately its own workspace, so `cargo check` at the repository root never
sees it — and the CoAP target was found in this pass to have been uncompilable since
15 September 2026. Rebuild rather than assume:

```bash
cd fuzz && rustup run nightly-2025-12-04 cargo fuzz build dns_message
```

(`cargo +nightly fuzz` does not work on this machine: asdf's shims precede `~/.cargo/bin` on
`PATH`, so `cargo` is not the rustup proxy and `+toolchain` is read as a subcommand name.)

## Choosing the record action by `query_type`

Told to "answer text queries for hello.test with the text netget-eval-ok", llama3.1:8b once
answered the TXT query with `send_dns_a_response` (`dns/txt-record` 4/5 in the committed eval
baseline). The `query_type` event parameter now maps each type to its action, and
`send_dns_a_response` / `send_dns_txt_response` each say which query type they answer. Wording
only: no executor changed, so the Stable evidence (`e2e_testing`, the six conditions below) is
unaffected. With seed 42 the case is 5/5.

