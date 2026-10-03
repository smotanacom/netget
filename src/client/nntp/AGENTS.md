# NNTP Client Implementation

## Overview

The NNTP (Network News Transfer Protocol) client implementation provides LLM-controlled access to Usenet newsgroups.
NNTP is a text-based protocol defined in RFC 3977 (and earlier RFCs 977, 2980) used for reading and posting articles to
distributed discussion systems.

## Library Choices

**No external dependencies** - NNTP is implemented using:

- `tokio::net::TcpStream` for network I/O
- `tokio::io::BufReader` for line-based reading
- Manual protocol implementation (text-based commands)

This approach was chosen because:

1. **Simplicity**: NNTP is a simple text protocol similar to SMTP/IMAP
2. **No mature Rust crate**: No suitable high-level NNTP client library exists
3. **LLM control**: Direct protocol control allows the LLM to construct any command
4. **Lightweight**: No additional dependencies beyond tokio

## Architecture

### Connection Model

```
┌─────────────┐         TCP          ┌─────────────┐
│             │◄────────────────────►│             │
│  NetGet     │   Text Protocol      │   NNTP      │
│  NNTP Client│   (Commands/Replies) │   Server    │
│             │                      │   (Usenet)  │
└─────────────┘                      └─────────────┘
```

### Protocol Flow

1. **Connection**: TCP connection to server (default port 119)
2. **Welcome**: Server sends `200` (read/post) or `201` (read-only) greeting
3. **Commands**: Client sends text commands terminated by CRLF
4. **Responses**: Server sends status code + text, multi-line for some commands
5. **Multi-line Data**: Terminated by `.` on a line by itself

### State Machine

```
ConnectionState:
  Idle ──────► Processing ──────► Accumulating
    ▲               │                    │
    │               │                    │
    └───────────────┴────────────────────┘
```

- **Idle**: No LLM call in progress, ready to process new data
- **Processing**: LLM call in progress, queue new data
- **Accumulating**: LLM still processing, continue queuing

### Multi-line Response Handling

NNTP commands that return multi-line responses include:

- **LIST** (215): List of newsgroups
- **ARTICLE** (220): Full article (headers + body)
- **HEAD** (221): Article headers only
- **BODY** (222): Article body only
- **XOVER** (224): Article overview information

The client detects multi-line responses by status code and reads until it encounters a `.` terminator.

## LLM Integration

### Event Types

1. **nntp_connected**
    - Fired when connection established
    - Includes: `remote_addr`, `welcome_message`
    - LLM decides: Initial command (LIST, GROUP, etc.)

2. **nntp_response_received**
    - Fired for each server response
    - Includes: `status_code`, `response`, `command` (that triggered it)
    - LLM decides: Next command or action

### Actions

#### Async Actions (User-triggered)

- `nntp_group`: Select a newsgroup (GROUP command)
- `nntp_article`: Retrieve full article (ARTICLE command)
- `nntp_head`: Retrieve article headers (HEAD command)
- `nntp_body`: Retrieve article body (BODY command)
- `nntp_list`: List newsgroups (LIST command)
- `nntp_xover`: Get article overviews (XOVER command)
- `nntp_post`: Post a new article (POST command)
- `nntp_stat`: Get article status (STAT command)
- `nntp_quit`: Disconnect (QUIT command)

#### Sync Actions (Response-triggered)

- `nntp_group`: Select newsgroup in response to data
- `wait_for_more`: Wait for more data before responding

### Action Execution

Most actions are converted to `ClientActionResult::Custom` with command strings:

```json
{
  "name": "nntp_command",
  "data": {
    "command": "GROUP comp.lang.rust"
  }
}
```

The `nntp_post` action follows the proper NNTP POST protocol flow:

1. Send `POST` command
2. Article data (headers + body) is stored in pending state
3. Server responds with `340 Send article to be posted`
4. Article data is automatically sent (headers + body + terminator)
5. Server responds with `240 Article received ok` or error code

### Dashboard injection (`[ nntp_group ]`, `[ nntp_list ]`, … `[ nntp_quit ]`)

`connect_with_llm_actions` registers a command channel
(`client::command_support::register_command_channel`) *before* the read-loop task and therefore
before the `nntp_connected` LLM call, which a manual rule can park. Because `read_line` is not
cancellation-safe, commands are drained by a separate `command_loop` task (registered with
`register_client_task`) that shares the write half, not by a `select!` arm. Every wire verb
yields `ClientActionResult::Custom` (`nntp_command` / `nntp_post`), which the generic
`handle_stream_client_command` cannot write, so `command_loop` routes the result through
`apply_action` — the one function the LLM path also uses to encode commands (including the
POST-then-340 article hand-off) — then records an `injected_action` access-log entry and
replies with `ClientSendOutcome`. An injected `nntp_quit` writes `QUIT` and half-closes; the
read loop sees EOF. The command handle is removed on every read-loop exit (missing welcome,
read error, EOF, injected quit) so the rail stops offering `[ send ]` on a dead client. Test:
`tests/client/nntp/command_channel_test.rs` (zero LLM calls).

## Response Codes

Common NNTP status codes:

- **200**: Server ready, posting allowed
- **201**: Server ready, posting not allowed
- **211**: Group selected (GROUP response)
- **215**: List of newsgroups follows (LIST response)
- **220**: Article retrieved (ARTICLE response)
- **221**: Headers retrieved (HEAD response)
- **222**: Body retrieved (BODY response)
- **224**: Overview information follows (XOVER response)
- **340**: Send article to be posted (POST intermediate)
- **411**: No such newsgroup
- **420**: Current article number is invalid
- **423**: No article with that number
- **430**: No article with that message-id
- **500**: Command not recognized
- **502**: Command not permitted

## Example Prompts

### List Available Newsgroups

```
Connect to NNTP at news.example.com:119 and list all newsgroups
```

LLM flow:

1. Receives `nntp_connected` event
2. Executes `nntp_list` action
3. Receives `nntp_response_received` with newsgroup list
4. Can parse and display results

### Read Articles from Group

```
Connect to NNTP at news.example.com:119, select comp.lang.rust, and retrieve the last 10 articles
```

LLM flow:

1. Receives `nntp_connected` event
2. Executes `nntp_group` with group_name="comp.lang.rust"
3. Receives `211` response with article range
4. Executes `nntp_xover` with range to get article list
5. Executes `nntp_article` for each article of interest

### Post Article

```
Connect to NNTP at news.example.com:119 and post a test article to test.misc
```

LLM flow:

1. Receives `nntp_connected` event
2. Executes `nntp_post` action with headers and body
3. POST command is sent, article data is stored pending 340 response
4. When 340 response is received, article is automatically transmitted
5. LLM receives final success (240) or error response

## Limitations

1. **No Authentication**: Current implementation doesn't support AUTHINFO (NNTP authentication)
2. **No Pipelining**: Commands are sent one at a time
3. **No Binary Support**: No support for binary attachments (yEnc, uuencode)
4. **Limited Error Handling**: Error responses are passed to LLM but not parsed structurally
5. **No Compression**: No support for COMPRESS or MODE STREAM
6. **No SSL/TLS**: No built-in support for NNTP over SSL (port 563)

## Future Improvements

1. **AUTHINFO Support**: Add username/password authentication
2. **STARTTLS**: Upgrade connection to TLS for security
3. **Binary Attachments**: Support yEnc decoding/encoding for binary data
4. **Response Parsing**: Parse structured responses (article numbers, ranges, etc.)
5. **Pipelining**: Send multiple commands without waiting for responses
6. **CAPABILITIES**: Discover server capabilities via CAPABILITIES command
7. **HDR/OVER**: Support newer HDR and OVER commands (RFC 3977)
8. **Better Error Recovery**: Automatic retry logic for transient errors

## Testing

See `tests/client/nntp/AGENTS.md` for testing strategy.

## References

- RFC 3977: Network News Transfer Protocol (NNTP)
- RFC 2980: Common NNTP Extensions
- RFC 977: Original NNTP specification (obsoleted by RFC 3977)

## Bounded response reading (October 2026 review)

Response lines now use `client::response_reader::read_response_line`, sharing the
existing bounded line decoder. The 64 KiB cap includes the line terminator; a partial
line at EOF is an error. Oversized or incomplete replies are not forwarded as successful
responses.
Dot-terminated responses additionally have an 8 MiB aggregate cap, require an exact
`.` terminator and undo dot stuffing. A peer closing before that terminator is a
framing failure, avoiding POP3's former EOF loop and NNTP's partial-success result.
Pure decoder tests are in `tests/client_review_regression_test.rs::text_responses`.

## Response completion deadlines (October 2026 follow-up)

Shared text readers allow an idle established connection to wait for its first response byte,
then enforce a 30-second absolute line-completion deadline. Dot-terminated bodies have a
30-second whole-response deadline in addition to their byte cap. NNTP greetings are bounded
from the first wait; HTTP CONNECT has a single deadline spanning status and headers. A framing
timeout is terminal because resuming an interrupted parse would misalign the stream.
