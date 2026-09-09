# Mercurial HTTP Protocol Implementation

Read-only Mercurial server speaking a subset of the HTTP wire protocol
(version 1) on hyper. The model controls repository *metadata* — capabilities,
heads, branch map, bookmark namespaces. It cannot control repository *content*:
`getbundle` always answers with an empty changegroup.

**State**: Experimental. **Privilege**: none; the default port 8000 is above
1024. **Spec**: [WireProtocol](https://www.mercurial-scm.org/wiki/WireProtocol),
[HttpCommandProtocol](https://wiki.mercurial-scm.org/HttpCommandProtocol).

## Commands

| Request | Event | Response format |
|---|---|---|
| `GET /<repo>?cmd=capabilities` | `hg_capabilities` | newline-separated capability names |
| `GET /<repo>?cmd=heads` | `hg_heads` | space-separated 40-char node IDs |
| `GET /<repo>?cmd=branchmap` | `hg_branchmap` | one `branch node…` line per branch |
| `GET /<repo>?cmd=listkeys&namespace=…` | `hg_listkeys` | `key\tvalue` per line |
| `GET`/`POST /<repo>?cmd=getbundle` | `hg_getbundle` | `HG10UN` + empty changegroup |
| `?cmd=unbundle`, `?cmd=pushkey` | — | `403`, push is not implemented |
| any other `cmd` | — | `404` |

## What the model sees and controls

Every event carries `repository` and `client_ip`; `hg_listkeys` adds
`namespace`, `hg_getbundle` adds the raw `heads` and `common` request arguments.

**Actions**: `hg_capabilities` (`capabilities`), `hg_heads` (`heads`),
`hg_branchmap` (`branches`), `hg_listkeys` (`keys`), `hg_send_bundle`
(`bundle_type`), and `hg_error` (`message`, `code`) which is accepted for every
event.

There are no async actions. `create_hg_repository`, `delete_hg_repository` and
`list_hg_repositories` used to be advertised; there was no repository store for
them to act on and their results were discarded.

### The server refuses to put nonsense on the wire

- **Node IDs are validated.** Anything that is not exactly 40 hex characters is
  dropped with a WARN. Models like to answer `"abc123..."`, which would leave the
  client with an unparseable head. `heads` also accepts a whitespace-separated
  string, not just an array.
- **Capabilities are filtered** to what this server implements (`branchmap`,
  `getbundle`, `listkeys`). Advertising `unbundle` would invite a push that gets
  a `403`; advertising `bundle2` would make the client negotiate a format this
  server never speaks. Dropped entries are logged.
- **`hg_send_bundle` has no data parameter.** It used to take `bundle_data`,
  whose string was written to the socket as-is — arbitrary text presented to the
  client as a changegroup. Generating a real changegroup means emitting revlog
  deltas, manifests and filelogs, which is not implemented, so the action now
  only chooses the (single supported) bundle type and the server emits a
  well-formed *empty* bundle: `HG10UN` followed by three empty chunk groups.

### Failure behavior

| Situation | Result |
|---|---|
| `hg_error` action | that HTTP status and message |
| Action for a different command, or no action | `500` carrying only a `WireFailure` category; `decision=fail_closed_no_action` naming what was expected |
| LLM/handler call fails, backend saturated | `503` + `Retry-After: 1`; `decision=fail_closed_overloaded` |
| LLM/handler call fails otherwise | `500` carrying only a category; `decision=fail_closed_llm_error` |
| Request body over `MAX_REQUEST_BODY_BYTES` (1 MiB) | `413`, nothing buffered past the cap |
| `heads` with no valid node | the null node `000…0` (an empty repository) |

The peer gets a category, the log gets the error: `WireFailure::text()` is a
`&'static str`, so nothing derived from the failure — backend URL, model name,
anyhow chain — reaches an `hg` client. `decision=` tags (as in
`src/server/radius/`) keep "the model refused" (`model_reject`), "the model said
nothing usable" (`fail_closed_no_action`) and "the call errored" apart in the log.

## Implementation

`mod.rs` — hyper service, query parsing, one handler per command, all routed
through `call_llm`, so script and static `event_handlers` work.
`actions.rs` — action and event definitions, capability filtering, bundle
construction.

**No storage**: nothing is written to disk, no `.hg` directory exists, nothing
shells out to `hg`, and no state survives a request beyond the per-connection list
of repository names used for the UI. A repository name from the URL only ever
becomes a `String`.

**Request bodies are bounded.** Only `getbundle` has a body, and it is read
through `http_body_util::Limited` at 1 MiB and refused with `413` beyond that. A
plain `collect()` let one unauthenticated client grow the process by whatever it
cared to send.

## Not implemented

Push (`unbundle`, `pushkey`), any non-empty changegroup, bundle2, stream clones,
the `batch` / `known` / `lookup` commands, compression (`HG10GZ`, `HG10BZ`),
phases and obsolescence markers, largefiles, authentication, and SSH transport.

A clone against this server therefore produces an **empty repository** at best.
It is useful as a honeypot, for exercising a client's metadata path, and for
logging what a client asks for — not for distributing code.

## Example prompts

```json
{"type": "open_server", "port": 8000, "base_stack": "mercurial",
 "event_handlers": [
   {"event_pattern": "hg_capabilities", "handler": {"type": "static",
     "actions": [{"type": "hg_capabilities", "capabilities": ["branchmap", "getbundle", "listkeys"]}]}},
   {"event_pattern": "hg_heads", "handler": {"type": "static",
     "actions": [{"type": "hg_heads", "heads": ["1234567890abcdef1234567890abcdef12345678"]}]}}]}
```

```
listen on port 8000 via mercurial. Repository 'hello-world': answer hg_heads with
one 40-character hex node, hg_branchmap with a 'default' branch pointing at it,
and hg_listkeys with no bookmarks.
```

## Verified

At the HTTP level with `curl` and static handlers (zero LLM calls), on
127.0.0.1: `capabilities` filtered `unbundle`/`bundle2` out of a handler that
asked for them; `heads` dropped a bogus `abc123...` and returned only the valid
node; `branchmap` and `listkeys` framed correctly; `getbundle` returned exactly
`48 47 31 30 55 4e` + twelve zero bytes; `?cmd=unbundle` returned `403`.

**Not verified against the `hg` client** — it is not installed on the
development machine. The command set and framing follow the wire protocol
documentation, but no real Mercurial client has ever spoken to this server, and
modern `hg` prefers bundle2 and the `batch` command, neither of which exists
here. Treat "works with hg" as unproven.

`tests/server/mercurial/` **is** declared in `tests/server/mod.rs` (behind
`#[cfg(feature = "mercurial")]`) and runs: five mocked e2e cases. This file claimed
the opposite — the mod.rs footgun was real when it was written and has since been
fixed tree-wide.
