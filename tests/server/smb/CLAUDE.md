# SMB Protocol Tests

**Protocol**: SMB2, dialects 2.0.2 and 2.1, over Direct TCP (MS-SMB2 2.1)
**Model**: always mocked (`.with_mock`, `wait_for_mocks`, `verify_mocks`), except
`inbound_limit_test.rs` and `peer_inject_test.rs`, which run in-process and count calls
**Run**: `./cargo-isolated.sh test --no-default-features --features smb --test server -- smb:: --test-threads=100`

Every request a test sends is framed with the 4-byte Direct TCP header — a zero byte and a
24-bit length — exactly as a real client frames it on port 445, and every response is read as
one such frame. `wire_util.rs` holds the framing (`nbss`, `read_frame_sync`, `read_frame`) and
a full set of request builders written from MS-SMB2, not from `src/server/smb/wire.rs`, so the
tests that use them check the server against the specification rather than against itself.

## The suites

| File | What it proves |
|---|---|
| `real_client_test.rs` | **Two real clients.** Samba's `smbclient` logs in anonymously (SPNEGO/NTLMSSP, two legs), runs `ls` and `get` of a 70 000-byte binary file; the listing, the size, the modified time, the "blocks available" line and the exact bytes are asserted, and the session, recorded through a relay, must read clean in the pcap oracle. The Python `smbprotocol` library logs in as a named guest over bare NTLMSSP, lists the share and reads the same file. Both **fail, naming the install command, when the client is absent.** |
| `header_layout_test.rs` | Every response builder in `server::smb::wire` puts each header field at its MS-SMB2 2.2.1.2 offset (MessageId at 24) and round-trips through the request parser; compound chains align `NextCommand`. A raw session covering every implemented command, a different MessageId on each request, is checked reply by reply and run through the pcap oracle. A WRITE before any session is refused and its payload consumed, so the NEGOTIATE after it is answered. |
| `inbound_limit_test.rs` | `MaxWriteSize` is advertised; a WRITE of exactly `MAX_WRITE_SIZE` reaches the model; `MAX_WRITE_SIZE + 1` in a whole frame is refused `STATUS_INVALID_PARAMETER` before the model and the connection stays in step; a frame announcing `MAX_MESSAGE_BYTES + 1` is refused after its header and the connection closes; a fresh connection is still served. |
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
  the EndOfFile of the CREATE response, so its mock gives `smb_create_file` a `size` and
  expects zero `query_info` calls; smbclient's mock gives none and expects one.
- **smbclient is run with `-U % -N`.** With `-N` alone it first logs in as the Unix user
  running the test with an empty password, is refused (no mock rule), and falls back to
  anonymous — two SESSION_SETUP exchanges whose first depends on the machine.
- **A READ is answered with the whole file each time**; the server slices the requested
  range. A read at or past the end is `STATUS_END_OF_FILE` and still costs a model call.
- **The session gate is upstream of the model.** A refused request must cost no call, and the
  tests say so with `expect_calls(0)` rather than by omitting the rule.

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
job's feature set.

## Known gaps

- Neither real client exercises WRITE; the raw-packet suites do.
- No test drives SMB's own read deadlines or connection cap; the shared ratchets
  (`tcp_server_bounds_ratchet_test.rs`, `accept_bounded_test.rs`) hold them.
- No fuzz target covers the request, compound or NTLMSSP parsers.
