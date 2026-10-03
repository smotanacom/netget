# RSS Feed Client (RSS 2.0)

Fetches feeds over HTTP and hands the model **structured items, never XML**. The model
chooses which feed to fetch and what to do with the result.

## History: why this was disabled for nine months

This client was born disabled. `e16ffb2d` (2025-11-09) added the RSS server *and* client but
commented the client out of `client_registry.rs` and `src/client/mod.rs` in the same commit —
"Temporarily disabled due to API mismatches". Re-enabled 2026-08.

The mismatches were real and had accumulated: it imported `crate::llm::client::OllamaClient`
and `crate::llm::llm_helpers::call_llm_for_client` (neither path exists), used
`tokio::sync::mpsc::Sender` where the status channel is an `UnboundedSender`, called
`app_state.clients.*` methods that were never on `AppState`, and its `Client::connect` read
`ctx.app_state` where the field is `ctx.state`. Porting it to `src/client/rip/`'s shape was
the whole fix; no protocol logic changed.

Two things were *removed* rather than ported:

- **`if_modified_since`** was a declared action parameter that nothing read. A declared but
  unread parameter is dead weight the model will try to use, so it went. Conditional requests
  are a real feature and would need real code.
- **The 5-second polling task** that did nothing but check whether the client still existed.

## Flow

```
open_client ──> resolve remote_addr, base_url = http://<remote_addr>
            ──> event rss_connected { base_url }
                 model answers fetch_rss_feed { url } / disconnect
                 (no fetch is invented — an empty answer leaves the client idle)

fetch ──> HTTP GET ──> parse RSS 2.0 ──> event rss_feed_fetched { structured items }
                                          model may fetch again, wait, or disconnect
```

Chained fetches are capped at **16 per client** (`MAX_CHAINED_FETCHES`), so a model that keeps
answering `fetch_rss_feed` cannot spin forever.

## Structured data, not XML

This is the point of the protocol. `channel_to_event_data` turns the parsed channel into:

```json
{
  "url": "http://host/news.xml",
  "feed_title": "...", "feed_link": "...", "feed_description": "...",
  "item_count": 2,
  "items": [{"title": ..., "link": ..., "description": ..., "author": ...,
             "pub_date": ..., "guid": ...}]
}
```

Models cannot reliably parse XML back out of a string, so handing them the raw document would
be the same mistake as putting raw bytes in an action parameter.

## URL resolution

`resolve_feed_url` accepts a bare path (`/news.xml`), a bare name (`news.xml`) or an absolute
URL, and joins the first two to the address the client was opened on. Getting this wrong sends
the request to the wrong host and looks like a network fault rather than a bug, which is why it
has its own test.

## Events and actions

| Event | Raised when | Actions offered |
|---|---|---|
| `rss_connected` | client initialised | `fetch_rss_feed`, `disconnect` |
| `rss_feed_fetched` | a feed parsed | `fetch_rss_feed`, `wait_for_more`, `disconnect` |

Both are emitted. Note that, on the client path, what the model is actually offered comes from
`get_async_actions()` — `call_llm_for_client` never reads `get_sync_actions()` or the event's
own action list. RSS is unaffected because `fetch_rss_feed` and `disconnect` are in both, but
see `src/client/tftp/CLAUDE.md` for the case where that difference bit.

## Connection model

RSS is request/response, not a persistent connection. "Connected" means the address resolved
and an HTTP client is ready. `Client::connect` must return a `SocketAddr`, so the remote
address is resolved with `lookup_host` at connect time — which also fails early on a bad
address instead of at first fetch.

## Limitations

- **RSS 2.0 only** — no Atom, no RSS 1.0/RDF.
- **No conditional requests** — no ETag, no If-Modified-Since (see above).
- **No autodiscovery** — the exact feed URL must be known.
- **No enclosures or categories** extracted.
- **No caching**; every fetch re-downloads.
- The whole document is buffered in memory.

## Example

```
Connect to localhost:8080 via rss and fetch /tech-news.xml, show me the latest 5 items
```

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running RSS client. The handle is
registered **before** the `rss_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human. This
also fixes a smaller problem: before, a model that answered the connect event with nothing
left a client with no tasks at all and no way to ask it for anything.

The fetch machinery now takes a `FetchCtx` instead of nine positional arguments, and splits
into `fetch_chain` (fetch → `notify_feed_fetched` → queue what comes back) and two entry
points over it: `fetch_loop`, the model's own chain from the connect path, which marks the
client `Disconnected` once it drains; and `injected_chain`, which does not — an injected
fetch is one operation on a client the operator is still holding open. `apply_action` decides
what one executed action means and is shared by all three callers, so `resolve_feed_url`
happens in exactly one place.

**Outcome semantics — `Executed`, never `Sent`.** reqwest owns the socket and reports no wire
byte count for the GET. The command loop **awaits** the fetch and reports
`Executed { detail: "fetch_rss_feed http://host/news.xml -> 2 items" }`, which proves the feed
was fetched *and* parsed; a failed fetch is an `Err`, an unknown action is `Rejected`,
`disconnect` is `Disconnected`. The `rss_feed_fetched` event fires from its own registered
task, so a manual rule parking that LLM call cannot wedge the command loop.
