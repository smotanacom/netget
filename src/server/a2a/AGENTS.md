# A2A agent — Experimental, Agent2Agent protocol 1.0, JSON-RPC binding

hyper HTTP/1.1. `GET /.well-known/agent-card.json` serves the card (name, description and
skills from startup parameters; one `supportedInterfaces` entry: `JSONRPC`, protocol `1.0`, the
URL built from the request's `Host`). `POST /` is JSON-RPC 2.0. Anything else is 404.

`model.rs` (shared with the client) holds the 1.0 shapes: protobuf-JSON camelCase messages
(`ROLE_USER`/`ROLE_AGENT`, parts `text`/`data`/`url`/`raw`), tasks with `TASK_STATE_*`, the
`StreamResponse` union (`task`, `message`, `statusUpdate`, `artifactUpdate`) and the A2A error
codes. Bounds: 1 MiB bodies, 64 parts, 256 KiB text, a JSON budget, 30 s header and body reads.

Refused by Rust before any handler runs: unparsable body (-32700), not JSON-RPC 2.0 (-32600),
`A2A-Version` absent or not 1.x (-32009 — a missing header means 0.3, which is not served),
unknown method (-32601), push-config methods (-32003), `SubscribeToTask` and the extended card
(-32004), streaming when the card does not advertise it (-32004), a message that is not a user
message with at least one known part (-32602).

- `a2a_message` (SendMessage / SendStreamingMessage) and `a2a_task_request` (GetTask,
  CancelTask, ListTasks) ask the handler for exactly one `a2a_reply`: a direct `message`, a
  `task`, a `tasks` list or a named `error`. Rust supplies ids, context ids and timestamps and
  checks the reply; a streamed task becomes SSE `task` → `statusUpdate` (working) → one
  `artifactUpdate` per artifact → final `statusUpdate`, which is the order a2a-sdk requires.
- No answer, two answers, an invalid answer or a backend failure is JSON-RPC **-32603** with
  `decision=fail_closed_*` / `model_silent` logged — never an invented message or task.

There is no task store: GetTask and friends are answered by the handler, which can keep state
in memory or SQLite. Not implemented: push notifications, `SubscribeToTask`, the extended
card, the HTTP+JSON and gRPC bindings, 0.3 compatibility, authentication schemes.
