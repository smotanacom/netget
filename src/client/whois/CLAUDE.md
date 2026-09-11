# WHOIS Client Implementation

## Overview

The WHOIS client provides LLM-controlled domain and IP address lookups using the WHOIS protocol (RFC 3912). WHOIS is a
simple TCP-based text protocol that runs on port 43.

## Protocol Details

**Port:** 43 (standard WHOIS)
**Transport:** TCP
**Format:** Plain text
**Pattern:** Single request-response (connection closes after response)

## Library Choices

**Network:** `tokio::net::TcpStream`

- Standard async TCP for connecting to WHOIS servers
- No external WHOIS libraries needed - protocol is very simple

**Protocol Implementation:** Direct TCP with manual text formatting

- Send: `<query>\r\n`
- Receive: Read until EOF (server closes connection)
- Response is plain text (no structured format)

## Architecture

### Connection Model

WHOIS is a one-shot protocol:

1. Client connects to server (port 43)
2. Client sends query (domain or IP) + "\r\n"
3. Server sends full response
4. Server closes connection

### LLM Integration Flow

```
User → open_client(whois, server:43, "Query example.com")
  ↓
1. TCP connect to server:43
  ↓
2. Call LLM with whois_connected event
  ↓
3. LLM returns query_whois(query="example.com")
  ↓
4. Send "example.com\r\n" to server
  ↓
5. Read full response until EOF
  ↓
6. Call LLM with whois_response_received event
  ↓
7. LLM parses response (registrar, dates, nameservers, etc.)
  ↓
8. Connection closes (status: Disconnected)
```

### State Machine

Unlike TCP/HTTP clients, WHOIS has no ongoing state:

- **Connected** → Send query → **Reading response** → **Disconnected**
- No idle/processing/accumulating states (single request only)

## LLM Control Points

### Actions

**Async Actions (user-triggered):**

- `query_whois` - Send WHOIS query for domain or IP
- `disconnect` - Close connection (usually not needed, auto-closes)

**Sync Actions (response to events):**

- The same two. A client has one LLM entry point, so the async/sync split cannot
  express a narrowing and `client_llm_action_set` unions them anyway — but
  `events::handler::action_catalog_for_pattern` builds the `event_handlers`
  validation catalog from the **sync** list plus the matching event's own actions,
  and reads the async list not at all. Declaring async-only meant
  `{"type": "query_whois"}` in a static handler was rejected as an unknown
  action, so a whois client could not be routed deterministically at all.

### Events

Both are declared as `LazyLock<EventType>` statics in `actions.rs`, and
`get_event_types()` returns **those statics**. It used to build two *different*
`EventType`s inline with the same ids, no parameters, no actions and
`{"type": "placeholder"}` as the example action — so the model was shown
`placeholder` as the way to answer, the real parameters were documented nowhere,
and the validation catalog above was the common actions alone. The WHOIS *server*
had the identical example bug and fixed it with a comment saying the example is
rendered verbatim into the documentation.

1. **whois_connected** - Triggers after connection established
    - LLM responds with `query_whois` action
    - Parameters: `remote_addr`

2. **whois_response_received** - Triggers when full response received
    - LLM parses text response
    - Parameters: `response` (text), `query` (the query that actually went on the
      wire, model's or injected), `truncated` (bool)
    - `truncated` is true when the server sent more than `MAX_RESPONSE_BYTES`
      (1 MB) and only the head is in `response`. A WHOIS reply carries no length
      field, so a model that read half a record and believed it had the whole one
      would answer confidently and wrongly.

### Custom Action Results

- `ClientActionResult::Custom { name: "whois_query", data: { query: String } }`
    - Sent from `query_whois` action
    - Triggers query transmission

### Dashboard injection (command channel)

The client registers a command channel (`client::command_support`) before the
connected-event LLM call, so `[ query_whois ]` and `[ disconnect ]` work while a
manual rule parks that event, and also after an LLM failure (the client now stays
connected in that case instead of dying). Commands are drained by a separate task
(shape 2 of the playbook) that shares the `Arc<Mutex<WriteHalf>>`; because
`query_whois` yields `ClientActionResult::Custom { "whois_query" }` the generic arm
cannot run it, so the task routes it through the same `apply_action` the LLM path
uses, records an `injected_action` access-log entry and replies `Sent{bytes}`.
The session reads until EOF with a cancellation-safe `read()` loop and raises
`whois_response_received` with whichever query actually went on the wire (model's
or injected; the first one wins, since RFC 3912 is one query per connection — a
second injected query is written but most servers ignore it). The handle is
removed when the session ends or on an injected disconnect, so the rail stops
offering `[ send ]` on a dead client. `tests/client/whois/command_channel_test.rs`
proves the path with zero LLM calls.

## Response Parsing

WHOIS responses are **unstructured text**. Format varies by registry:

**Domain WHOIS (example.com):**

```
Domain Name: EXAMPLE.COM
Registrar: Example Registrar, Inc.
Creation Date: 1995-08-14T04:00:00Z
Expiration Date: 2025-08-13T04:00:00Z
Name Server: ns1.example.com
Name Server: ns2.example.com
Status: clientTransferProhibited
```

**IP WHOIS (8.8.8.8):**

```
NetRange: 8.0.0.0 - 8.255.255.255
CIDR: 8.0.0.0/8
NetName: LVLT-ORG-8-8
NetHandle: NET-8-0-0-0-1
Organization: Level 3 Parent, LLC (LPL-141)
```

**LLM Parsing:** The LLM receives raw text and extracts relevant fields using pattern matching.

## Referrals

Some WHOIS servers redirect queries to authoritative servers:

**Example (querying whois.iana.org for example.com):**

```
refer: whois.verisign-grs.com
```

**Current Implementation:** No automatic referral following
**Future:** LLM could detect "refer:" and trigger new query to referred server

## WHOIS Servers

**Default Servers:**

- **IANA:** whois.iana.org (root registry, provides referrals)
- **Domain TLDs:**
    - .com/.net: whois.verisign-grs.com
    - .org: whois.pir.org
    - .io: whois.nic.io
- **IP Registries:**
    - ARIN (Americas): whois.arin.net
    - RIPE (Europe): whois.ripe.net
    - APNIC (Asia-Pacific): whois.apnic.net

**Server Selection:** User specifies server in `remote_addr` parameter

## Logging Strategy

**Dual Logging (tracing + status_tx):**

- **INFO:** Connection events (connected, disconnected)
    - `"WHOIS client 1 connected to whois.iana.org:43"`

- **DEBUG:** Query/response events
    - `"WHOIS client 1 querying: example.com"`
    - `"WHOIS client 1 received 1234 bytes"`

- **TRACE:** Full response text
    - `"WHOIS response:\n<full text>"`

- **ERROR:** Connection/read failures
    - `"WHOIS client 1 failed to send query: <error>"`

## Limitations

1. **No Structured Parsing:** Response is plain text, LLM must parse manually
    - Different registries use different formats
    - No standard schema (unlike DNS)

2. **No Referral Following:** If server returns "refer: <other-server>", manual re-query needed
    - Could be enhanced: LLM detects referral and opens new client

3. **Rate Limiting:** Many WHOIS servers rate-limit queries
    - Excessive queries may result in temporary IP bans
    - Best practice: Cache results, avoid repeated queries

4. **Server Availability:** Some WHOIS servers are unreliable
    - Timeouts, connection refused, incomplete responses
    - LLM should handle gracefully
    - **How much a server sends is entirely its choice**, because a WHOIS reply
      has no length field — "read until EOF" is the framing. Both read paths are
      therefore capped at `MAX_RESPONSE_BYTES` (1 MB) and bounded by
      `RESPONSE_READ_TIMEOUT` (60s between reads, so a server still sending keeps
      its connection). An over-long reply is truncated, flagged and answered
      rather than abandoned: the head of a WHOIS record is the part with the
      registrar in it. Before that, the main loop grew a `Vec` for as long as the
      remote end kept writing, and the follow-up path used `read_to_string`,
      which is unbounded *and* fails the whole transfer on the first non-UTF-8
      byte — a real hazard on a referral chase, which points this client at
      registrars nobody here chose and several of which still emit Latin-1.

5. **Single Query Per Connection:** WHOIS closes after one response
    - Cannot reuse connection for multiple queries
    - Need new client for each query

## Security Considerations

**Privacy:** WHOIS exposes domain registration information

- Registrant names, emails, addresses (unless redacted)
- Use responsibly, respect privacy

**Abuse:** WHOIS can be used for reconnaissance

- Domain enumeration, registrant tracking
- Consider ethical implications

## Example Prompts

**Basic Domain Query:**

```
Connect to WHOIS at whois.verisign-grs.com:43 and query "example.com"
```

**IP Lookup:**

```
Connect to WHOIS at whois.arin.net:43 and find information about 8.8.8.8
```

**Referral Following (manual):**

```
Query whois.iana.org for "example.com", then follow the referral to query the authoritative server
```

## Testing Notes

**Everything runs on loopback against NetGet's own WHOIS server.** No test here
contacts a public WHOIS server, and none may: the repo rule is "bind to localhost
only; never contact external endpoints".

| File | Covers |
|---|---|
| `tests/client/whois/e2e_test.rs` | the model-driven round trip — connect event → `query_whois` static handler → the record read to EOF → the `whois_response_received` event the model actually sees, asserted field by field |
| `tests/client/whois/command_channel_test.rs` | dashboard injection (`[ query_whois ]` / `[ disconnect ]`), zero LLM calls |

`e2e_test.rs` used to hold three tests that queried `whois.iana.org` and
`whois.verisign-grs.com`. They were `#[ignore]`d for it — correctly, since they
also had no `.with_mock()` and so needed a real Ollama — which meant they ran
nowhere and asserted nothing while still reading as coverage of the client's
model-driven path, which had none. They are now one test that runs.

The event assertion is possible because the mock's response generator executes
**inside** the test, so what it captures is the model's actual view rather than a
reconstruction of it. That is what caught the placeholder/validation defects
above: the static handler naming `query_whois` was *rejected* until the events
declared their actions.

## Future Enhancements

1. **Automatic Referral Following:** Detect "refer:" in response, open new client
2. **Response Parsing Library:** Structured WHOIS response parser
3. **Multi-Query Support:** Connection pooling for multiple queries
4. **RDAP Support:** Modern alternative to WHOIS (RFC 7480, JSON-based)
