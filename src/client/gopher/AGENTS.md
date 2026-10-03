# Gopher Client (RFC 1436)

Browses a gopher hole under LLM control: fetch a selector, get the reply back
**already parsed**, decide what to fetch next.

**State**: `Experimental`. **Privilege**: none. **Stack**: `ETH>IP>TCP>GOPHER`.
The server half is `src/server/gopher/` (Beta, verified against real `curl`) —
read its CLAUDE.md too; it records what a real client actually does on the wire.

## One connection per request

Gopher has no session. The client sends one selector line, the server answers
and **closes** — the close is the framing, because the reply carries no length
and no content type. There is therefore no persistent socket for this client to
hold, and "browsing" means opening a new connection per item.

That is what happens. `connect()` opens **one** connection to `remote_addr`,
proves the hole is reachable, takes the local address the `Client` trait must
return, and closes it without sending anything. Every fetch afterwards — the
model's or an injected one — opens its own connection to whatever host and port
the action names.

The probe is not free ceremony. Without it a mistyped address produces a client
that reports success and then silently fails at its first fetch; with it,
`open_client` fails where the operator is looking. The server sees a peer that
hung up before asking for anything and logs exactly that.

**Why a fetch names its own host and port**: a menu item carries them, so a
gopherspace legitimately spans servers. Omit both and the fetch goes to the
address the client was opened against, which is what makes the first request
easy to write.

## Menu or document: the *requesting* item's type decides

Nothing in a Gopher reply says which it is. This is the client's real problem
and it is handled explicitly rather than by sniffing: `send_gopher_request`
carries an `item_type`, and `parse_gopher_reply` reads the reply according to
it. `1` and `7` are menus; everything else is a document.

**The one exception is not a heuristic about content.** A reply whose first line
is a **type-3 item** is reported as `gopher_error_received` whatever was asked
for, because type 3 is the only error channel the protocol has and a server uses
it to refuse *any* request. The test is structural — a leading `3` **and** the
tab-separated fields of a menu line — so a document that merely begins with the
digit 3 is still a document. `tests/client/gopher/e2e_test.rs` pins both halves.

### When the guess is wrong

Nothing is thrown away, and the mismatch is visible to the model:

| Situation | What happens |
|---|---|
| Menu asked for, **no** line parses as a menu line | `gopher_document_received`, with `requested_item_type` still `1`/`7` — the model can see it asked for the wrong thing |
| Menu asked for, *some* lines do not parse | `gopher_menu_received`; the odd lines are kept verbatim in `malformed_lines` |
| Document asked for, reply is really a menu | Reported as a document, tabs and all. **Deliberately not re-read as a menu**: a document may legitimately be full of tabs, so guessing here would corrupt real documents to rescue a mislabelled one |

## The follow-up chain, and why it must raise events

Browsing is iterative by construction: fetch a menu, pick an item, fetch that,
repeat. A follow-up fetch that raised no event would give the model exactly one
turn and then leave it deaf — the `elasticsearch`/`http2` defect the root
`CLAUDE.md` catalogues, and for a *browser* it is fatal rather than merely
lossy, because a menu the model cannot act on is the whole product.

So every fetch raises its event and asks the model again, and the chain is
bounded by a **depth**, not by silence: `MAX_FOLLOWUP_DEPTH` (6). Hitting it
logs at WARN and posts to the status stream. The recursive call is boxed
(`Pin<Box<dyn Future + Send>>`) because an `async fn` awaiting itself has an
infinitely-sized future, and `+ Send` is named explicitly because the chain is
awaited inside a `tokio::spawn`, where inference will not supply it.

`result.actions` is executed on every path. That is the single most common
client defect in this repo; `tests/client_event_wiring_test.rs` is the ratchet.

## Events

All three are raised by `mod.rs` and all three are covered by the e2e suite.

| Event | Carries |
|---|---|
| `gopher_menu_received` | `selector`, `host`, `port`, `item_count`, `items[]`, optional `malformed_lines[]`, optional `truncated` |
| `gopher_document_received` | `selector`, `host`, `port`, `requested_item_type`, `text`, `line_count`, optional `truncated` |
| `gopher_error_received` | `selector`, `host`, `port`, `message` |

**A menu item is a structured object, never a pre-formatted blob**:
`{item_type, item_type_name, display, selector, host, port}`. Following a link
is echoing those four fields back in a `send_gopher_request`, which is exactly
what the e2e test does rather than hardcoding the target — that is the only way
to prove the tab-separated line really became fields.

`item_type_name` exists because the model should not have to know that `7` means
"search server" or that `i` is not a link. The client recognises **every** type
the RFC and common practice define, unlike the server, which deliberately emits
only a closed set: a client has no choice about what it is handed.

A document's `text` has had the terminating `.` line removed, CRLF turned into
newlines, and the doubled leading period ("periodating") undone — RFC 1436 makes
that the client's job. `curl` does not do it, which is why the server suite
asserts the doubled form and this one asserts the single.

## Actions

| Action | Effect |
|---|---|
| `send_gopher_request` | Fetch one item. `selector` defaults to empty (the root menu), `item_type` to `1`, `host`/`port` to the client's own address |
| `send_gopher_search` | A type-7 search: `<selector>\t<query>`. Always answers with a menu |
| `wait_for_more` | Stop the automatic browse and idle |
| `disconnect` | End the browsing session |

All four are declared **once**, in `get_async_actions()`. `get_sync_actions()`
is empty and no event type attaches actions of its own. A client has one LLM
entry point and therefore cannot express a narrowing, so
`client_llm_action_set` = async ∪ sync ∪ event actions is this list either way;
the ~40 clients that duplicate their list into both methods were working around
a bug that is fixed.

**`wait_for_more` does not mean "the reply was partial"** — a Gopher reply
cannot be partial, since EOF is the framing. It means "nothing further from me:
end the automatic browse and leave the client idle until someone sends it
another request". It is included because every stream client's model reaches for
it, and a name the model is punished for using is worse than a name with a
narrower meaning.

**`disconnect` closes no socket**, because between requests there is none. It
marks the session finished, stops the client acting on its own, and takes it out
of the dashboard's send list.

`selector` refuses CR, LF and tab; `query` refuses CR and LF but **allows** tab,
because RFC 1436 splits the request line on the first tab only. A rejected value
surfaces as an action failure — the request-side equivalent of the server's
field sanitizing, and the same reasoning: otherwise the model forges a second
request, or turns a plain fetch into a search, by accident.

## Command channel (the dashboard's `[ send ]`)

Registered **before** anything that can call the model. A dashboard-created
client gets a `*` → manual rule, so the first event this client raises parks for
as long as the operator takes; without the handle in place first, `[ send ]`
would be greyed out for exactly the window in which it is wanted.

A separate task, not a `select!` arm, for the same reason WHOIS uses one: an
injected request must not queue behind a parked model call.
`command_support::handle_stream_client_command` cannot run this vocabulary —
there is no persistent write half to hand it and the fetch verbs yield
`ClientActionResult::Custom` — so injected actions go through the same
`perform_fetch`/`notify` pair the model's do.

**The reply is sent as soon as the bytes have been exchanged, before the model
is told about them.** Telling the model can park for minutes behind a manual
handler and `send_to_client` has a caller-supplied timeout; the caller asked
whether the request went out, which is answerable immediately. The consequence
is that a second injected command waits in the bounded channel until the first
one's follow-up chain finishes — ordinary "client busy" backpressure.

An injected fetch **does** feed the model afterwards, at depth 0. The operator
opening a menu should get the same follow-up behaviour the model's own fetch
gets; a side channel that browses without telling the model would be the same
defect in a different costume.

## No `gopher_connected` event, and what that costs

Gopher gives a client nothing to report on connect: no banner, no capabilities,
no session. So the opening turn is the initial-instruction call
(`event: None`), which `llm_budget::call_llm_for_client` deliberately does not
route through `event_handlers` — there is no event id to match a pattern
against.

The cost is real and worth knowing: **a deterministic first fetch cannot be
expressed as a static handler.** Express it by injecting the action instead
(`AppState::send_to_client`), or give the client an instruction. Everything
after the first fetch routes normally. The upside is that a dashboard-created
client's `*` → manual rule cannot park client creation itself, which is the
trap `ClientForm` adds zero-action `<proto>_connected` rules to avoid.

A client with no instruction does not fail: it logs, skips the opening call, and
waits for an injected request.

## Startup parameters

None declared, and none read. Host and port come from `remote_addr`; everything
else is per-request and belongs in the action, not in a knob set once at
startup.

## Bounds

| | |
|---|---|
| `MAX_REPLY_BYTES` | 1 MiB, then `truncated: true` on the event. Gopher sends no length, so without a cap any server can make the client allocate until it dies |
| `MAX_MENU_ITEMS` | 4096 |
| `MAX_FOLLOWUP_DEPTH` | 6 model round-trips per chain |
| connect / read timeout | 15 s / 60 s |

## Not implemented

- **Gopher+** — the `+` attribute protocol, `!`/`$` requests, `+INFO` blocks. A
  fifth-and-beyond field on a menu line is ignored rather than parsed.
- **`gophers`** (Gopher over TLS). No RFC; curl supports it, this does not.
- **Binary item transfer.** Types `4`/`5`/`6`/`9`/`g`/`I` are *listed* in a
  parsed menu and can be asked for, but the reply is decoded as text — there is
  no download path, deliberately, because handing bytes to the model would mean
  base64 in event data, which the repo rules forbid.
- **CSO (type 2)** and **telnet/tn3270 (types 8, T)**: listed, not followed.

## Why `Experimental`, and what would earn `Beta`

`Beta` means "works against real clients" — for a *client* protocol, read that
as a real independent **server**. The e2e suite's peer is NetGet's own Gopher
server, so every wire assertion is **same-project evidence**: it shows the two
halves of this repo agree, not that either agrees with RFC 1436. That is the
circular-evidence class the root `CLAUDE.md` names by name, and it is the whole
of why the rating is not higher.

**What would earn Beta**: one test in which a third-party Gopher daemon serves a
menu and a document on loopback and this client parses both.

Checked, since the constraint matters: **the software exists and the
loopback-only rule is not what blocks it.** Real serveable daemons — `geomyidae`
(suckless, C), `gophernicus`, `pygopherd`, `bucktooth` (Floodgap's own, Perl) —
all bind an ordinary TCP port and are perfectly happy on 127.0.0.1 with a
throwaway document root, so no test would ever need to touch the real
gopherspace. None of them is installed on this machine, and that is the actual
cost: the binary has to exist wherever the suite runs.

Copy `npm`'s shape when doing it — its real-CLI test **fails** when the binary
is absent, saying in as many words that skipping "would leave NPM's maturity
rating resting on nothing". Four protocols (`kubernetes`, `oci_registry`,
`maven`, `websocket`) sit at Experimental precisely because their real-client
test prints `SKIP:` and returns `Ok(())`, which on a runner without the binary
is a silent pass. A skip-gated Gopher test would buy this client nothing.
