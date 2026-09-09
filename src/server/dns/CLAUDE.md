# DNS Protocol Implementation

## Overview

DNS (Domain Name System) server implementing RFC 1035 standard for domain name resolution. The LLM can respond to
queries with A, AAAA, CNAME, MX, TXT records, and NXDOMAIN responses using structured actions.

**Status**: Beta (Core Protocol)
**RFC**: RFC 1035 (Domain Names), RFC 3596 (AAAA), RFC 1034 (Concepts)
**Port**: 53 (UDP), declared as `PrivilegeRequirement::PrivilegedPort(53)`

### What "Beta" covers here

Answers built by the structured actions are RFC 1035 responses that a real stub
resolver will accept: correct wire format, the client's transaction ID echoed,
and the question section repeated. Not covered: TCP transport, EDNS0, DNSSEC,
multi-record answers, and record types outside A/AAAA/CNAME/MX/TXT (see
Limitations).

E2E coverage lives in `tests/server/dns/` - `test.rs` (note: `test.rs`, not
`e2e_test.rs` as most protocols use), `dig_test.rs` and `llm_failure_test.rs`.
All six pass; this file claimed until September 2026 that the four in `test.rs`
"currently fail before they reach the DNS layer" over a `DocumentationRequired`
retry, which was stale.

**`dig_test.rs` is the one that makes the Beta rating mean something.** `test.rs`
drives the server with hickory-client while the server encodes with
hickory-proto - the same project, the same codec - so on its own it proves the
wire format self-consistent rather than correct. That is the circularity that
kept `rss` at Experimental until `feed-rs` did its parsing. ISC BIND's `dig`
shares no code with hickory, checks that the transaction id it chose comes back
and that the question matches what it asked, and **fails rather than skipping**
when the binary is absent (the `npm` precedent).

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
- `text` (required) - Text data to return
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
- Ollama lock serializes LLM API calls

### Scripting Compatibility

DNS protocol is excellent candidate for scripting:

- Repetitive request/response pattern
- Deterministic responses based on domain/query type
- No complex state machine
- High query volume typical use case

When scripting enabled:

- Server startup generates Python/JavaScript script (1 LLM call)
- All subsequent queries handled by script (0 LLM calls)
- Dramatically improves throughput

## References

- [RFC 1034: Domain Names - Concepts and Facilities](https://datatracker.ietf.org/doc/html/rfc1034)
- [RFC 1035: Domain Names - Implementation and Specification](https://datatracker.ietf.org/doc/html/rfc1035)
- [RFC 3596: DNS Extensions to Support IPv6 (AAAA)](https://datatracker.ietf.org/doc/html/rfc3596)
- [hickory-dns Documentation](https://docs.rs/hickory-proto/latest/hickory_proto/)
- [DNS Query Types (IANA)](https://www.iana.org/assignments/dns-parameters/dns-parameters.xhtml#dns-parameters-4)

## Failure behaviour: SERVFAIL, never silence

When `call_llm` returns `Err` — backend down, overloaded, or no usable response — the query is
answered with **SERVFAIL (RCODE 2)** rather than dropped. `actions::build_servfail` copies the
transaction ID and the question section off the request and clears AA; a stub resolver that gets
this stops waiting and moves to the next nameserver, where silence costs it a full per-server
timeout (5s in glibc) first.

The ID and question echo are not optional decoration: a resolver discards a response that fails
either check, which turns the SERVFAIL back into silence. See §2b above — this is the same
requirement the successful actions have, and it regressed once before (`6a384617`).

Covered by `tests/server/dns/llm_failure_test.rs`, which asserts the RCODE nibble on the raw
bytes as well as through a decoder.
