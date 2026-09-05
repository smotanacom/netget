# XMPP E2E Testing

`tests/server/xmpp/test.rs`. Two tests, **6 LLM calls total**.

`tests/server/xmpp/peer_inject_test.rs` and `tests/server/xmpp/llm_failure_test.rs` cost **zero**
LLM calls: the first injects actions through the dashboard's peer handle, the second points the
backend at a dead port so every `call_llm` errors.

## Strategy

The peer parses everything the server writes with **`xmpp-parsers` 0.22** — already a
dependency of the `xmpp` feature, and the stanza layer every `tokio-xmpp`-based client is built
on. So a stanza that is not well-formed, lands in the wrong namespace, or had text interpolated
into it without XML escaping fails the test the same way it would break a real client's stream.

It is not a full client. `tokio_xmpp::Client` cannot connect, because the server has no
STARTTLS and no SASL exchange. Say that rather than implying an XMPP client was run.

`StreamPeer` handles the awkward part: an XML stream is one element that never closes, so it
cannot be parsed as it stands. Appending `</stream:stream>` to whatever has arrived turns it
into a document, and a parse failure just means the stanza is still incomplete — the same
incremental-parse state a real client is in. `wait_for_children(n, …)` reads until the stream
parses with at least `n` top-level stanzas, and panics with the raw bytes on timeout.

## What is asserted

| Test | Asserts |
|---|---|
| `test_xmpp_stream_header_and_features` | The XML declaration is present (RFC 6120 4.2); root is `<stream/>` in `http://etherx.jabber.org/streams` with `from`, `id`, `version='1.0'`; `<stream:features/>` decodes into `StreamFeatures` and its `sasl_mechanisms.mechanisms` are exactly what the model listed, in order. `from` is **omitted** by the action, so this also proves the `domain` startup parameter is read rather than the hardcoded default |
| `test_xmpp_message_and_presence_round_trip` | The message element inherits `jabber:client`; `Message` decodes with both JIDs (including the resource) and `type='chat'`; the body survives byte-for-byte through escaping and re-parsing, with `&`, `<`, `>` and an apostrophe in it. `Presence` decodes with `type_ == Type::None` — RFC 6121 4.7.1: an available presence carries no `type` attribute, and `type='available'` is not a legal value — plus `show` and `status` |

The body characters are the load-bearing part: interpolated unescaped they produce a malformed
stream, and a real client's parser drops the whole stream on the first well-formedness error
rather than skipping one stanza.

## LLM call budget

- `test_xmpp_stream_header_and_features`: 1 startup + 1 stream-open event = 2
- `test_xmpp_message_and_presence_round_trip`: 1 startup + 3 stream events = 4

## Mock notes

Every event is `xmpp_data_received` with the whole accumulated buffer in `xml_data`; rules are
distinguished by `and_event_data_contains("xml_data", …)` on `stream:stream`, `<message` or
`<presence`. The server hands the model raw text and clears its buffer only once the model has
acted on it, so a rule that never matches leaves the buffer in place and every later event
carries the earlier text too.

## `llm_failure_test.rs` — the backend-failure path

The LLM endpoint is `http://127.0.0.1:1`, where nothing listens, so `call_llm` errors on the very
first `xmpp_data_received` event. The test asserts the peer is **answered rather than left
hanging**: a fatal `<stream:error/>` (RFC 6120 §4.9) preceded by the synthesised opening stream
tag §4.9.1.1 requires, carrying `<internal-server-error/>` for a `WireFailure::Unavailable`
(`<resource-constraint/>` is the `Overloaded` counterpart), a `<text/>` equal to one of the two
`WireFailure` categories verbatim, a closing `</stream:stream>`, and then EOF.

It also scans the whole raw byte stream for the tokens that leaked in the original incident —
`LLM`, `ollama`, `11434`, a backend URL, a filesystem path, `✗`, `retries`. That scan is the
point of the test; the stanza assertions only prove the frame is one a real client can parse.

## Not covered

SASL authentication, IQ stanzas (roster, bind), stream restart after auth, STARTTLS, multiple
stanzas in one write, and malformed input from the client side.

## Privacy

127.0.0.1 only, no external connections, works offline.
