# IMAP Protocol E2E Tests

## Test Overview

Tests IMAP4rev1 server with raw TCP clients validating RFC 3501 command/response sequences,
including authentication, mailbox operations, and message retrieval — plus **two real
third-party clients**, which are where the maturity rating actually rests:

| file | peer | what only it covers |
|---|---|---|
| `e2e_client_test.rs` | `async-imap` 0.11 (Rust) | LIST, EXAMINE, STATUS, NOOP, concurrent sessions, LOGIN failure |
| `real_client_test.rs` | python3 stdlib `imaplib` | the literal **byte** count, the `CAPABILITY` command, `select()`'s `EXISTS` return |
| `test.rs` | hand-written TCP | the command/response shapes, trimmed |

## `real_client_test.rs` — the second client

One client can agree with one bug. `async-imap` was the only real peer this server had ever
faced, and a rating resting on one client rests on that client's leniency — elsewhere in this
repository `etcd`, `grpc` and `mysql` were each Beta on a single client and each turned out to
be unusable by every other conformant implementation.

`imaplib` is python's standard library: no install, no C, no line of code shared with
`async-imap`. The test **fails rather than skips** when python3 is absent, and is not
`#[ignore]`d.

It drives one session — greeting, `CAPABILITY`, `LOGIN`, `SELECT`, `SEARCH`, `FETCH`, `LOGOUT`
— through a python driver that prints what imaplib *parsed* as JSON, which the Rust side then
asserts on. The driver is told nothing about what to expect, so a wrong answer arrives as a
wrong value rather than as a passing test.

Three things it reaches that nothing else here does:

- **The literal byte count.** The fetched body carries `ü`, `ß` and an em dash, so its byte
  length (146) and character length (142) differ. `imaplib` reads *exactly* `n` bytes and then
  resumes line parsing, so a `{n}` counted in characters desynchronises the connection.
- **`CAPABILITY` the command**, not the greeting's `[CAPABILITY ...]` code. `imaplib.__init__`
  issues a real one and refuses to proceed without `IMAP4REV1`. The mock answers the command
  with a strict superset of the greeting (it adds `NAMESPACE`), so the assertion can tell the
  two apart.
- **`select()`'s return value**, which is `untagged_responses.get('EXISTS', [None])` — the
  count, not the tagged line.

**Verified non-vacuous, and note which half stayed correct both times:**

| break | what imaplib reported |
|---|---|
| `body.len()` → `body.chars().count()` in `execute_send_imap_fetch` | FETCH completed `OK`; `m.fetch()` returned `('OK', [None])` — no literal at all |
| the `* n EXISTS` line removed from `execute_send_imap_select` | SELECT completed `OK`; `m.select()` returned `('OK', [None])` |

In both cases the **tagged completion was still right**. A test asserting only on `typ == 'OK'`
would have passed against both broken servers.

## Test Strategy

- **Consolidated per command type** - Each test focuses on a specific IMAP command
- **Multiple server instances** - 10 separate servers (one per test)
- **Real TCP clients** - Manual socket I/O with tagged command/response pattern
- **No IMAP library** - Tests use `tokio::net::TcpStream` directly
- **Helper functions** - `send_imap_command()` and `read_greeting()` abstract protocol details

## LLM Call Budget

- `test_imap_greeting()`: 1 startup call (greeting)
- `test_imap_capability()`: 1 startup call + 1 CAPABILITY command
- `test_imap_login()`: 1 startup call + 1 LOGIN command
- `test_imap_login_failure()`: 1 startup call + 1 LOGIN command
- `test_imap_select_mailbox()`: 1 startup call + 2 commands (LOGIN, SELECT)
- `test_imap_list_mailboxes()`: 1 startup call + 2 commands (LOGIN, LIST)
- `test_imap_fetch_message()`: 1 startup call + 3 commands (LOGIN, SELECT, FETCH)
- `test_imap_search()`: 1 startup call + 3 commands (LOGIN, SELECT, SEARCH)
- `test_imap_logout()`: 1 startup call + 1 LOGOUT command
- `test_imap_noop()`: 1 startup call + 2 commands (LOGIN, NOOP)
- `test_imap_status()`: 1 startup call + 2 commands (LOGIN, STATUS)
- `imaplib_completes_a_session_against_the_imap_server()`: 1 startup + 1 greeting + 1
  CAPABILITY + 1 LOGIN + SELECT/SEARCH/FETCH/LOGOUT = **8**
- **Total: 40 LLM calls** (12 startups + 28 command calls)

## Scripting Usage

**Scripting Disabled** - IMAP tests use action-based responses only

- IMAP protocol is highly stateful (authentication, mailbox selection)
- Script generation not beneficial for complex command sequences
- LLM interprets each command with full session context

## Client Library

**Manual TCP Client** - No IMAP library used

- `tokio::net::TcpStream` for connections
- Helper `send_imap_command(tag, command)` handles request/response cycle
- Helper `read_greeting()` reads initial `* OK` response
- Tagged responses parsed to verify command completion

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~120-180 seconds for full test suite
- Slower due to 32 LLM calls and complex protocol

## Failure Rate

- **Medium** (10-20%) - LLM struggles with IMAP format complexity
- Common issues:
    - Missing or incorrect tagged responses
    - Untagged responses without tagged completion
    - Incorrect capability formatting
    - FETCH response literal syntax errors

## Test Cases

1. **test_imap_greeting** - Validates `* OK` greeting with IMAP4rev1
2. **test_imap_capability** - Tests CAPABILITY command and response parsing
3. **test_imap_login** - Tests successful LOGIN with correct credentials
4. **test_imap_login_failure** - Tests LOGIN rejection with wrong credentials
5. **test_imap_select_mailbox** - Tests SELECT command and mailbox status (EXISTS, RECENT, FLAGS)
6. **test_imap_list_mailboxes** - Tests LIST command for mailbox hierarchy
7. **test_imap_fetch_message** - Tests FETCH with FLAGS and BODY[]
8. **test_imap_search** - Tests SEARCH ALL command
9. **test_imap_logout** - Tests LOGOUT command with BYE response
10. **test_imap_noop** - Tests NOOP keep-alive command
11. **test_imap_status** - Tests STATUS command for mailbox info

## Known Issues

- **Helper function complexity** - `send_imap_command()` must parse multi-line responses
- Tests terminate on tagged response (A001 OK/NO/BAD)
- Some LLMs send untagged data without final tagged response
- Timeout issues if LLM generates verbose explanations instead of protocol

## Example Test Pattern

```rust
// Start server
let server = start_netget_server(ServerConfig::new(prompt)).await?;
wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

// Connect and read greeting
let mut client = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;
let greeting = read_greeting(&mut client).await?;
assert!(greeting.starts_with("* OK"));

// Send tagged command
let responses = send_imap_command(&mut client, "A001", "CAPABILITY").await?;

// Verify untagged response
let cap_line = responses.iter().find(|l| l.starts_with("* CAPABILITY")).expect("...");
assert!(cap_line.contains("IMAP4rev1"));

// Verify tagged completion
let ok_line = responses.iter().find(|l| l.starts_with("A001 OK")).expect("...");
```

## Helper Function: send_imap_command

Handles IMAP request/response cycle:

1. Sends formatted command: `{tag} {command}\r\n`
2. Reads lines until tagged response appears
3. Returns all responses (untagged + tagged)
4. Handles multi-line responses (literals, flags)

## Helper Function: read_greeting

Reads initial server greeting:

1. Reads single line from stream
2. Expects `* OK [CAPABILITY ...] Server Ready`
3. Returns greeting string for validation

## `literal_framing_test.rs` — the `{N}` count must match the octets that follow

Ten tests, **zero LLM calls**: `ImapProtocol::execute_action` is driven directly, because the
question is what the executor will let onto the wire, not what a model would ask for.

The suite's own FETCH fixture declared `{50}` for a 46-octet body and passed for as long as it
existed — every assertion in `test.rs` is `line.contains("FETCH")` on a **trimmed** string, and
trimming discards exactly the framing RFC 3501 §4.3 cares about. The pcap oracle found it;
`validate_imap_literals` is the same check on our side of the socket, so a model that miscounts
is refused rather than obeyed.

Refused (each fails if the guard is removed):

- `{50}` over 46 octets — the original defect. Long enough to exist, so a length check alone
  would pass it; octet 50 lands inside `A004`.
- `{40}` over 46 octets — lands mid-`Hello`.
- `{9000}` with six octets after it — the client would block forever.
- a literal whose payload consumes the response to its last octet, leaving the line
  unterminated.

Accepted (each passes with or without the guard, which is what makes them controls — a guard
that refused every literal would satisfy the four above and take IMAP's literal vocabulary
with it):

- the corrected 46-octet fixture, asserted **byte for byte** including the terminating CRLF;
- a literal followed by a space and another attribute, which is what `send_imap_fetch` emits
  when `BODY[]` is not the last item;
- `{46}` in free text with no CRLF after it, and malformed braces — not literal markers;
- a `{999}\r\n` sequence *inside* a body, which the walk must skip over rather than read.

