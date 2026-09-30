# SMB Protocol Tests

**Protocol**: SMB2, dialects 2.0.2 and 2.1, over Direct TCP (MS-SMB2 2.1)
**Model**: always mocked. The suites that spawn the binary use `.with_mock`, `wait_for_mocks`,
`verify_mocks`; `inbound_limit_test.rs`, `bounds_test.rs`, `failure_modes_test.rs` and
`peer_inject_test.rs` run the server in-process against a counting mock (or static handlers)
**Run**: `./cargo-isolated.sh test --no-default-features --features smb --test server -- smb:: --test-threads=100`

Every request a test sends is framed with the 4-byte Direct TCP header — a zero byte and a
24-bit length — exactly as a real client frames it on port 445, and every response is read as
one such frame. `wire_util.rs` holds the framing (`nbss`, `read_frame_sync`, `read_frame`) and
a full set of request builders written from MS-SMB2, not from `src/server/smb/wire.rs`, so the
tests that use them check the server against the specification rather than against itself.

## The suites

| File | What it proves |
|---|---|
| `real_client_test.rs` | **Two real clients, counted verb by verb from the recorded bytes.** Samba's `smbclient` logs in anonymously (SPNEGO/NTLMSSP, two legs) and runs `ls`, `get` of a 70 000-byte binary file, `put` of a 100 000-byte one (two WRITEs), `mkdir`, `rm`, `echo`, `tdis` and `logoff`. The Python `smbprotocol` library logs in as a named guest over bare NTLMSSP and runs `listdir`, a read of the same file, `stat` (a related compound of CREATE, five QUERY_INFOs and CLOSE), a write in two WRITEs, an explicit FLUSH, `mkdir`, `echo` and the pool teardown. Each session goes through a recording relay; the test prints every command sent with every NTSTATUS it was answered with, asserts each expected verb was sent and answered STATUS_SUCCESS, asserts the listing, sizes, times and downloaded bytes, reassembles the model's `write` events by offset and asserts they are exactly the uploaded bytes, asserts smbclient's `rm` reached the model as `delete_on_close`, and runs the whole session through the pcap oracle. Both **fail, naming the install command, when the client is absent.** |
| `header_layout_test.rs` | Every response builder in `server::smb::wire` puts each header field at its MS-SMB2 2.2.1.2 offset (MessageId at 24) and round-trips through the request parser; compound chains align `NextCommand`. A raw session covering every command the server answers — including FLUSH, IOCTL (refused), SET_INFO and LOCK (not supported) and a CANCEL shown unanswered — a different MessageId on each request, is checked reply by reply and run through the pcap oracle. A WRITE before any session is refused and its payload consumed, so the NEGOTIATE after it is answered. |
| `bounds_test.rs` | Every declared bound but the two in `inbound_limit_test.rs`, each at the bound and one past it, each verified by removal: the three read deadlines through their startup parameters and the first-message one at its 30s default; `MAX_CONNECTIONS` with a slot returned by a peer hanging up and by a server-side close while the peer holds on; sessions, trees and open handles per connection, with the slots that come back; `MaxReadSize` and `MaxTransactSize`; `MAX_COMPOUND_REQUESTS` (the model asked exactly 32 times for 40 CREATEs) and `MAX_RESPONSE_FRAME_BYTES` (17 1 MiB READs answered in one frame); `MAX_CREDIT_GRANT`, keep-alives, the RFC 1002 session request's 256-byte bound, short frames and SMB1. |
| `inbound_limit_test.rs` | `MaxWriteSize` is advertised; a WRITE of exactly `MAX_WRITE_SIZE` reaches the model; `MAX_WRITE_SIZE + 1` in a whole frame is refused `STATUS_INVALID_PARAMETER` before the model and the connection stays in step; a frame announcing `MAX_MESSAGE_BYTES + 1` is refused after its header and the connection closes; a fresh connection is still served. |
| `failure_modes_test.rs` | `src/server/smb/CLAUDE.md`'s failure sections as assertions: each of session_setup, create, read, write, query_info and query_directory, when the model rejects, says nothing, or cannot be reached, is refused on the wire (ACCESS_DENIED for the model's refusals, INTERNAL_ERROR for an outage, ACCESS_DENIED for every refused login) and leaves a log line naming the path or user with the right `decision=` token. The overload classifier maps every `RateLimitError` to INSUFFICIENT_RESOURCES. |
| `e2e_test.rs` | NEGOTIATE, guest SESSION_SETUP allowed and denied (`STATUS_ACCESS_DENIED`), concurrency, a binary READ decoded from base64, literal text without `encoding`, a directory handle's attribute and its refused READ, WRITE refused without `smb_write_file` and accepted with it, CREATE before any session refused without a model call, and the action-routing and payload-codec unit checks. |
| `llm_failure_test.rs` | A CREATE and a READ whose model call fails get an SMB2 ERROR (`STATUS_INTERNAL_ERROR`) correlated to their own MessageId, TreeId and SessionId, and the connection survives to answer a CLOSE. |
| `e2e_llm_test.rs` | Prompt-shaped scenarios with a mocked model; the auth ones assert the wire status. |
| `peer_inject_test.rs` | The dashboard's peer handle: an injected wire verb writes nothing (a reply needs a request to correlate with), `close_connection` disconnects, and the session's own exit path releases the handle and counts the bytes it read. Zero model calls. |

## Mock expectations worth knowing

- **One rule per event, branching on the event** where a test opens more than one path:
  `create` rules use `respond_with_actions_from_event` to answer `/` (the share root) with
  `smb_create_directory` and a file with `smb_create_file`. Two rules on `create` would have
  the first answer both.
- **smbclient's `get` asks `query_info`; smbprotocol never does.** smbprotocol reads exactly
  the EndOfFile of the CREATE response, so its mock gives `smb_create_file` a `size`, and its
  `stat` is answered from the handle; smbclient's mock gives none, so `get` asks.
- **smbclient's `rm` lists before it deletes.** It sends QUERY_DIRECTORY with the file name as
  the pattern and opens only what the listing names, so the listing rule includes
  `upload.bin` when that is the pattern — the model remembering the upload.
- **smbclient sends LOGOFF only when told to.** At the end of `-c` it disconnects the tree and
  drops the socket; the session ends with `tdis; logoff` so both verbs are driven. It has no
  command that issues FLUSH, which is why FLUSH rests on smbprotocol and the raw session.
- **smbclient is run with `-U % -N`.** With `-N` alone it first logs in as the Unix user
  running the test with an empty password, is refused (no mock rule), and falls back to
  anonymous — two SESSION_SETUP exchanges whose first depends on the machine.
- **A READ is answered with the whole file each time**; the server slices the requested
  range. A read at or past the end is `STATUS_END_OF_FILE` and still costs a model call.
- **The session gate is upstream of the model.** A refused request must cost no call, and the
  tests say so with `expect_calls(0)` rather than by omitting the rule, or by counting calls.
- **An outage is a request that matches no rule.** `failure_modes_test.rs` and
  `llm_failure_test.rs` give the approving and refusing answers rules and the failing ones
  none; the mock answers HTTP 500, which is the shape of a backend outage. A catch-all
  `on_any()` would answer them and the test would prove nothing.

## Handshake the raw tests use

NEGOTIATE (MessageId 0), a one-step guest SESSION_SETUP with an empty security buffer
(allocates session 1), TREE_CONNECT to `\\127.0.0.1\share` (allocates tree 1), then file
operations addressed to tree 1 of session 1. A WRITE's data starts at `DataOffset` 112 — the
first byte of the StructureSize-49 body's buffer — with no padding byte before it.

## Requirements

- `smbclient` — `brew install samba` (macOS) or `apt-get install -y smbclient`.
- `smbprotocol` — `python3 -m pip install smbprotocol`.
- `tshark` — the pcap oracle fails, rather than skips, without it.

`registry-audit` in `.github/workflows/ci.yml` installs all three and runs
`smb::real_client_test` in its real-client evidence loop. `smb` is not in the blocking `test`
job's feature set, so no blocking job runs any of this suite; run it yourself.

## Known gaps

- CANCEL and IOCTL are driven only by the raw session: neither client issues them against a
  guest session (smbprotocol's FSCTL_VALIDATE_NEGOTIATE_INFO is turned off because there is no
  key to sign it with). Both are refusal or silence paths.
- FLUSH is driven by smbprotocol and the raw session; smbclient has no command that sends it.
- No Windows, macOS or Linux kernel client has been run against the server.
