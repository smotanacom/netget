# A2A client — Experimental, Agent2Agent protocol 1.0, JSON-RPC binding

`connect` fetches `http://<remote_addr>/.well-known/agent-card.json` through the shared
`http_fetch` client (no redirects), requires a `JSONRPC` interface for protocol `1.0`, and
refuses a card whose URL names a different host or port than `remote_addr` unless
`allow_card_redirect` is set — a card must not be able to send the client somewhere else.
`a2a_connected` carries the card's name, description, skills and streaming flag.

Actions build the request and the `A2A-Version: 1.0` header in Rust: `a2a_send_message`
(SendMessage, or SendStreamingMessage when `stream` is true and the card streams),
`a2a_get_task`, `a2a_cancel_task`, `a2a_list_tasks`. Each answer is checked against the 1.0
shapes and its JSON-RPC id, then raised as `a2a_response` with the result or error, the task id
and last state; a stream is collected whole (1 MiB, 256 events, 30 s) into `stream_events`.
Shares `src/server/a2a/model.rs`. JSON-RPC binding only; no push notifications.
