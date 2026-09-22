# NNTP E2E Test Documentation

## Overview

End-to-end tests for NNTP (Network News Transfer Protocol) server implementation. Tests verify newsgroup listing, group
selection, article retrieval, and article overview functionality using raw TCP connections.

## Test Strategy

### Black-Box Testing

- Use raw TCP connections (no NNTP client library)
- Send NNTP commands as plain text
- Parse responses manually
- Verify response codes and multi-line format

### Test Coverage

1. **Basic Newsgroups** (`test_nntp_basic_newsgroups`)
    - Server greeting (200/201)
    - LIST command (newsgroup listing)
    - GROUP command (newsgroup selection)
    - ARTICLE command (article retrieval)
    - QUIT command (graceful disconnect)

2. **Article Overview** (`test_nntp_article_overview`)
    - GROUP command (select newsgroup)
    - XOVER command (article overview with tab-separated fields)
    - Multi-line response parsing

### LLM Call Budget

**Target**: < 10 LLM calls per test suite
**Actual**: ~6 LLM calls total

Breakdown:

- `test_nntp_basic_newsgroups`: ~4 LLM calls
    1. Greeting (connection opened)
    2. LIST command
    3. GROUP command
    4. ARTICLE command
       (QUIT doesn't require LLM call - can be hardcoded response)

- `test_nntp_article_overview`: ~2 LLM calls
    1. Greeting (connection opened)
    2. XOVER command

**Efficiency Techniques**:

- Single server instance per test
- Comprehensive prompts covering multiple commands
- LLM generates responses on-the-fly (no pre-populated data needed)
- Simple test scenarios (3 newsgroups, 5 articles)

## Test Execution

### Prerequisites

- Ollama running locally (required for LLM)
- Port availability (tests use dynamic port allocation)
- Release binary built: `./cargo-isolated.sh build --release --features nntp`

### Running Tests

```bash
# Run NNTP E2E tests only
./cargo-isolated.sh test --no-default-features --features nntp --test server::nntp::e2e_test

# With output
./cargo-isolated.sh test --no-default-features --features nntp --test server::nntp::e2e_test -- --nocapture

# Single test
./cargo-isolated.sh test --no-default-features --features nntp --test server::nntp::e2e_test test_nntp_basic_newsgroups
```

### Expected Runtime

- **Per test**: 5-15 seconds
- **Total suite**: 10-30 seconds

Factors affecting runtime:

- LLM response time (2-5s per call)
- Server startup time (1-3s)
- Network I/O (negligible, localhost only)

## Test Details

### test_nntp_basic_newsgroups

**Purpose**: Verify core NNTP functionality (LIST, GROUP, ARTICLE)

**Flow**:

1. Start NetGet with NNTP prompt
2. Connect via TCP to port
3. Read greeting (expect 200 or 201)
4. Send LIST command
    - Expect 215 response
    - Read multi-line newsgroup list
    - Verify 3 newsgroups present
5. Send GROUP comp.lang.rust
    - Expect 211 response with article count
6. Send ARTICLE 1
    - Expect 220 response
    - Read multi-line article (headers + body)
    - Verify headers present
7. Send QUIT
    - Expect 205 response
    - Connection closes

**Verification**:

- Response codes match NNTP spec
- Multi-line responses end with ".\r\n"
- Newsgroup names in LIST output
- Article has headers (Subject, From, etc.)

**LLM Calls**: ~4

### test_nntp_article_overview

**Purpose**: Verify XOVER command (article overview)

**Flow**:

1. Start NetGet with NNTP prompt
2. Connect via TCP
3. Read greeting
4. Send GROUP comp.test (select newsgroup)
5. Send XOVER 1-5 (get overview for articles 1-5)
    - Expect 224 response
    - Read multi-line tab-separated overview
    - Verify tab-separated format
6. Send QUIT

**Verification**:

- 224 response code for XOVER
- Tab-separated fields in overview
- At least 1 article in response

**LLM Calls**: ~2

## Known Issues

### 1. No Real NNTP Client Library

**Issue**: Using raw TCP instead of NNTP client library
**Impact**: Manual parsing of responses (more brittle)
**Rationale**: No mature Rust NNTP client library available, raw TCP is sufficient for testing

### 2. Limited Command Coverage

**Issue**: Only tests LIST, GROUP, ARTICLE, XOVER commands
**Missing**: HEAD, BODY, STAT, NEXT, LAST, POST, CAPABILITIES, etc.
**Rationale**: Core commands sufficient for basic validation, can expand later

### 3. Single Connection Testing

**Issue**: Tests don't verify concurrent connections
**Impact**: No load testing or race condition detection
**Future**: Add concurrent connection test

### 4. No Authentication Testing

**Issue**: Tests don't verify AUTHINFO commands
**Impact**: Authentication not validated
**Rationale**: AUTHINFO not implemented yet (see CLAUDE.md)

## Response Format Reference

### Single-Line Response

```
<code> <text>\r\n
```

Example: `200 NetGet NNTP Service Ready\r\n`

### Multi-Line Response

```
<code> <text>\r\n
<line 1>\r\n
<line 2>\r\n
.\r\n
```

Example (LIST):

```
215 list of newsgroups follows\r\n
comp.lang.rust 50 1 y\r\n
comp.lang.python 100 1 y\r\n
.\r\n
```

Example (ARTICLE):

```
220 <msg-id> article follows\r\n
Subject: Test\r\n
From: user@example.com\r\n
\r\n
Article body here.\r\n
.\r\n
```

### Common Response Codes

- **200**: Service ready, posting allowed
- **201**: Service ready, posting not allowed
- **205**: Goodbye (QUIT response)
- **211**: Group selected (count low high name)
- **215**: List of newsgroups follows
- **220**: Article follows (headers + body)
- **224**: Overview information follows (XOVER)
- **411**: No such newsgroup
- **423**: No such article in group
- **500**: Command not recognized

## Performance Notes

### Test Efficiency

- **Minimal LLM calls**: < 10 per suite
- **Fast execution**: < 30 seconds total
- **Localhost only**: No external network access
- **Dynamic ports**: No port conflicts

### Optimization Strategies

1. **Comprehensive prompts**: Cover multiple commands in one prompt
2. **Single server**: Reuse server instance for multiple commands
3. **Simple data**: Small newsgroup counts, short articles
4. **Hardcoded responses**: QUIT response doesn't need LLM

## Future Enhancements

1. **Add More Commands**:
    - HEAD (headers only)
    - BODY (body only)
    - STAT (article exists check)
    - NEXT/LAST (article navigation)
    - POST (article submission)

2. **Error Handling**:
    - Test invalid commands (expect 500)
    - Test non-existent newsgroups (expect 411)
    - Test non-existent articles (expect 423)

3. **Concurrent Connections**:
    - Multiple simultaneous clients
    - Verify connection isolation

4. **Authentication**:
    - AUTHINFO USER/PASS commands
    - Test authentication success/failure

5. **Performance Testing**:
    - Large newsgroup lists
    - Large article counts
    - Stress testing with many connections

## References

- [RFC 3977: NNTP Specification](https://datatracker.ietf.org/doc/html/rfc3977)
- [RFC 2980: Common NNTP Extensions](https://datatracker.ietf.org/doc/html/rfc2980)
- NetGet test infrastructure: `/tests/helpers.rs`
- NNTP implementation: `/src/server/nntp/CLAUDE.md`

## `connection_bounds_test.rs` — 5 in-process tests, **0 LLM calls** (the backend is a dead port)

The read deadlines in `src/server/nntp/mod.rs` — `FIRST_COMMAND_READ_TIMEOUT` (300s) and `IDLE_BETWEEN_COMMANDS_TIMEOUT` (600s) — driven from the wire. No
mock: these assert on *clocks*, not on answers, and a reachable backend would only add noise.
Loopback only.

**What each test is for.** A peer that has connected and said nothing must eventually be let go
of, because nothing else in the process will close that socket. A connection whose answer is
parked for a human must **not** be let go of, which is what stops the lazy fix of wrapping the
answer in the deadline as well as the read. The two bounds are different claims, so one test
drives a connection into the post-answer state and checks it is governed by `idle_timeout_secs`
rather than by the first-byte one; the connection past `MAX_CONNECTIONS` is refused in NNTP's own words and then a clean EOF.

**The last test is the regression, and it is deliberately the slow one.** The first-byte bound
was 60 seconds, and NetGet's own NNTP client is precisely a peer that bound stranded:
`src/client/nntp/mod.rs` reads the welcome line in its read loop and writes nothing until an action or `[ send message ]` says to, and a client made from the dashboard is routed `*` → manual — so it connects and
waits for a person, who gets 300 seconds (`src/state/intercepts.rs`). It is now 300s. Proving
that means holding a silent peer open **past 60 seconds with no startup parameters passed at
all**, so the wait cannot be made cheaper than the claim.

Every other test passes a short override instead of waiting the default out, which is also what
proves `first_byte_timeout_secs` and `idle_timeout_secs` are read rather than merely declared:
a parameter that was ignored would leave the 300-second default in force and the test would time
out. Each bound was verified by removing it and watching its test fail, and the default was
verified by putting 60 back and watching the regression test fail.
