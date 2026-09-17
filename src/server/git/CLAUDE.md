# Git Smart HTTP Protocol Implementation

Read-only Git server (`git clone`, `git fetch`, `git ls-remote`) speaking Smart
HTTP protocol v0 over hyper. The model describes a repository as ordinary
structured data — a branch, a commit message, a list of `{path, content}` files
— and the server compiles that into real Git objects and a real pack file.

**State**: Beta. The evidence is `tests/server/git/e2e_test.rs`, which is **not**
`#[ignore]`d and does **not** skip when git is missing (`run_git_command` returns
`Err` if the binary cannot be spawned, and every caller propagates it): the real
`git` binary completes a Smart HTTP clone, `git fsck --full` validates the pack
we sent, and `git show HEAD:README.md` asserts exact blob bytes. This file said
"Experimental" long after `metadata()` said `Beta`; the code was right.
**Privilege**: none; the default port 9418 is above 1024. **Spec**: [Smart HTTP](https://git-scm.com/docs/http-protocol),
[pack protocol](https://git-scm.com/docs/pack-protocol),
[pkt-line](https://git-scm.com/docs/protocol-common#_pkt_line_format).

## Endpoints

| Request | Meaning |
|---|---|
| `GET /<repo>/info/refs?service=git-upload-pack` | reference discovery, raises `git_info_refs` |
| `POST /<repo>/git-upload-pack` | object transfer, raises `git_upload_pack` |
| `…/git-receive-pack`, `?service=git-receive-pack` | `403` — push is not implemented |
| anything else | `404` |

`<repo>` is the first path segment (`/hello-world.git/info/refs` →
`hello-world.git`); a request with no leading segment reports `default`.

## What the model sees and controls

**Events**: `git_info_refs` (`repository`, `user_agent`, `client_ip`) and
`git_upload_pack` (`repository`, `wants`, `haves`, `capabilities`, `client_ip`).

**Actions** — the same two answer both events:

- `git_repository` — `files` (required array of `{path, content, executable?}`),
  `branch`, `commit_message`, `author_name`, `author_email`, `timestamp`.
- `git_error` — `message`, `code` (HTTP status; `404` for a missing repository).

There are no async actions. `create_git_repository`, `delete_git_repository` and
`list_git_repositories` used to be advertised; there was no repository store for
them to act on and their results were discarded, so they did nothing at all.

### Object IDs are computed, never supplied

The model is not asked for SHAs, and there is no parameter that accepts pack
bytes or base64 — that rule (no encoded bytes in action parameters) is what
forced this design, and it is also what makes the protocol work. `pack.rs` hashes
the blobs, builds the trees, writes the commit, and the SHA advertised by
`info/refs` is by construction the SHA of the commit inside the pack.

The old design asked the model for `pack_data` as base64. No model can emit a
valid pack (zlib streams, SHA-1 trailer), and the advertised SHAs were invented
independently of it, so a `git clone` against this protocol could never have
succeeded.

### Determinism is the one thing to get right

A clone is **two HTTP requests**, each answered separately. If
`git_upload_pack` returns different content than `git_info_refs` did, the commit
hash differs and git fails with `did not send all necessary objects`.

- A **static or script handler** answers both identically — this is the
  guaranteed-correct configuration, and it costs zero LLM calls.
- An **instruction** must pin the file contents exactly, and even then two model
  round-trips can disagree.
- `timestamp` defaults to a fixed constant (`DEFAULT_COMMIT_TIMESTAMP`), not
  "now", because the commit time is part of the hash.

When the SHAs do disagree the server logs an ERROR naming both hashes and the
cause, because git's own message does not.

### Failure behavior

| Situation | Result |
|---|---|
| `git_error` action | that HTTP status and message; git prints `remote: Error: …` |
| Invalid path (`..`, absolute, `.git`, NUL) or file/dir collision | `500` naming the offending path |
| No `git_repository` and no `git_error` | `500` carrying only a `WireFailure` category; `decision=fail_closed_no_action` |
| LLM/handler call fails, backend saturated | `503` + `Retry-After: 1`; `decision=fail_closed_overloaded` |
| LLM/handler call fails otherwise | `500` carrying only a category; `decision=fail_closed_llm_error` |
| `POST /git-upload-pack` body over `MAX_UPLOAD_PACK_BYTES` (1 MiB) | `413`, nothing buffered past the cap |
| Advertised commit ≠ packed commit | pack is still sent, ERROR logged; git aborts |

**The peer gets a category, the log gets the error.** `WireFailure::text()` is a
`&'static str`, so the backend URL, the model name and our own retry machinery
cannot reach a `git clone`'s terminal. `decision=` tags keep the three failure
shapes greppable apart (`fail_closed_*` vs the model's own refusal,
`decision=model_reject`), as in `src/server/radius/`. A saturated backend is
distinguished from a broken one because a bare `500` would have git record a
permanent fault where `503` + `Retry-After` tells it to come back.

## Implementation

`mod.rs` — hyper service, routing, event construction, response framing.
`pack.rs` — SHA-1, Git objects, tree building, pack v2 writer.
`pktline.rs` — pkt-line encoding and `git-upload-pack` request parsing.

**No storage.** Nothing is written to disk, no `.git` directory exists, no
repository state survives a request, and nothing shells out to `git` or links
`git2`/`gix`. Each request rebuilds the objects from the snapshot supplied for
that request; a repository name from the URL only ever becomes a `String` in an
event, never a path. (The Git **client**, `src/client/git/`, is the opposite by
design: it drives libgit2 against real directories.)

**Request bodies are bounded.** `POST /git-upload-pack` is read through
`http_body_util::Limited` at 1 MiB and refused with `413` beyond it. A plain
`collect()` would let one unauthenticated client grow the process by whatever it
cared to send — the shape of the `nfsserve` pre-auth DoS in the root
`CLAUDE.md`. The wire *is* length-prefixed (pkt-line), and `pktline.rs` never
allocates from that length: it slices the buffer it already has and stops at the
first header that runs past the end.

**SHA-1 is implemented in `pack.rs`.** The `sha1` crate is a dev-dependency of
this workspace and is not linked into the binary; Git object IDs are SHA-1 by
definition, so the algorithm is inlined. Verified against `git hash-object`.

**Pack objects use stored (uncompressed) deflate.** Git requires a zlib stream
but not that it be compressed, and `flate2` is optional in this workspace and not
enabled by the `git` feature. The zlib container, stored blocks and Adler-32 are
written by hand. Packs are therefore slightly larger than the input.

**Capabilities advertised**: `no-progress agent=netget symref=HEAD:refs/heads/<branch>`.
`side-band-64k` is deliberately *not* advertised: the multiplexed framing is only
correct if the server knows the client selected it, and git may compress the
`git-upload-pack` request body (it does not for small requests, but nothing
guarantees that). Refusing the capability keeps every response in the one framing
that is always right. The cost is no progress or error side-channel.

**Negotiation**: a round that sends `have` lines without `done` is answered with
`NAK` alone; anything else gets `NAK` followed by the full pack. There is no
common-ancestor computation, so a fetch always transfers everything.

## Connection bounds

Before September 2026 this server accepted without limit and bounded no read in time, so a peer
that connected and said nothing held a socket, a connection task and an `AppState` entry
forever, pre-authentication, on a server that would happily accept a hundred more. It now
declares both halves; the constants and the reasoning live beside them in `src/server/git/mod.rs`.

| Bound | Value | Why this number |
|---|---|---|
| `FIRST_BYTE_READ_TIMEOUT` | 30s | Smart HTTP is client-speaks-first, so a peer that has sent nothing has begun no request. Enforced with `TcpStream::peek` before the socket reaches hyper, so the request line is still there afterwards. |
| `IDLE_BETWEEN_REQUESTS_TIMEOUT` | 900s | The wait *between* requests, not a bound on a transfer. A clone is two requests on one keep-alive connection; the pack is built into a `Full<Bytes>` before the response is returned, so hyper writes an already-complete body and a slow reader is draining bytes rather than idling. git sets no keep-alive interval of its own; a client that has taken fifteen minutes to decide on its next request has gone away. |
| `MAX_CONNECTIONS` | 256 | Refusal: **HTTP/1.1 `503 Service Unavailable` with `Retry-After`** — Smart HTTP *is* HTTP, so `git` surfaces it as `The requested URL returned error: 503` rather than as an unexplained reset. |

**The deadline bounds the silence, not the transfer.** hyper owns every read once
`serve_connection` starts and keeps polling for frames *while a request is being answered*, so a
deadline on reads would be wrong here rather than merely awkward. The idle bound is a watchdog
over `ConnectionActivity` instead, which reports a connection with a request in flight as not
idle at all — so building a repository, including an LLM round-trip or a `manual` rule parked for
a human (`src/state/intercepts.rs`, 300s by default), can never close the connection it is an
answer for.

`tests/server/git/connection_bounds_test.rs` drives both halves from the wire: a peer that says
nothing is closed at the bound, and a peer that speaks is still served 38 seconds later. Removing
the `tokio::time::timeout` around the `peek` makes the first test hang for its whole 70-second
window and fail. `tests/tcp_server_bounds_ratchet_test.rs` fails the build if either bound is
removed, and `tests/accept_bounded_test.rs` covers the shared helper.

## Not implemented

Push (`git-receive-pack`), multiple commits or any history, tags, annotated tag
objects, symlinks, submodules, binary file content (`content` is a UTF-8 string),
deltas/thin packs, shallow and partial clones, protocol v2, the dumb HTTP
protocol, authentication, and the SHA-256 object format.

## Example prompts

Deterministic (recommended — no LLM calls, clone always succeeds):

```json
{"type": "open_server", "port": 9418, "base_stack": "git",
 "event_handlers": [{"event_pattern": "*", "handler": {"type": "static",
   "actions": [{"type": "git_repository", "branch": "main",
     "files": [{"path": "README.md", "content": "# Hello World\n"}]}]}}]}
```

Honeypot that refuses everything but logs every attempt:

```json
{"type": "open_server", "port": 9418, "base_stack": "git",
 "event_handlers": [{"event_pattern": "*", "handler": {"type": "static",
   "actions": [{"type": "git_error", "message": "Repository not found", "code": 404}]}}]}
```

LLM-driven:

```
listen on port 9418 via git. Serve repository 'hello-world' on branch main with
README.md containing exactly '# Hello World'. Answer git_info_refs and
git_upload_pack with the identical git_repository action every time.
```

## Verified

Against `git` 2.54 with a static handler on 127.0.0.1:

- `git clone http://127.0.0.1:PORT/hello-world.git` succeeds; nested paths
  (`src/main.rs`), the executable bit and file contents all survive; `git fsck`
  reports no problems.
- `git ls-remote` returns `HEAD` and `refs/heads/main` at the same SHA.
- `git fetch` / `git pull` on the resulting clone succeed.
- A blob hash produced by `pack.rs` equals `git hash-object` for the same bytes.
- `git_error` → `remote: Error: Repository not found` + `fatal: … not found`.
- Deliberately mismatched snapshots → `fatal: remote did not send all necessary
  objects`, with the server-side ERROR naming both hashes.
- `../escape.txt` → `500` naming the rejected path.

`tests/server/git/` is declared at `tests/server/mod.rs:71-72` and runs; the suite is 5/5
green against the action surface above. It was briefly red after that redesign because its
mocks still answered with the removed `git_advertise_refs` / `git_send_pack`, repaired in
`80b0bf0f`.

Those mocks had a second, older defect worth knowing about: they matched on prompt *substrings*
(`"Git client is requesting references"`) that appear in no prompt template — stale from the day
they were written. They now match on `.on_event("git_info_refs" | "git_upload_pack")`, the raw
event id the mock harness extracts, which cannot drift with prompt wording.
