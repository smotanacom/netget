# NSQ Broker Implementation

nsqd's TCP protocol (V2), as nsqd 1.3 speaks it. The model is the broker's judgement: it accepts
or refuses each publish and subscription, and it decides which messages a subscriber receives.
NetGet writes every byte of framing, assigns message ids and timestamps, and holds deliveries to
the client's RDY count.

**State**: Beta (see Maturity). **Privilege**: `None` — the well-known port is 4150.
**Stack**: `ETH>IP>TCP>NSQ`. **Feature**: `nsq` (no dependencies beyond serde_json).

## Library choice

None. The protocol is newline-terminated commands with size-prefixed bodies one way and
size/type-framed responses the other; `wire.rs` is pure functions. No maintained Rust crate
implements nsqd's side (the Rust NSQ crates are clients).

## Files

| File | What it holds |
|---|---|
| `mod.rs` | accept loop, the session loop (commands, heartbeats and injected actions in one `select!`), `Flow` (the connection's subscription, RDY, in-flight messages and pending queue), the three event paths, `Framer` |
| `wire.rs` | `parse_command`, `encode_command`, the frame renderers, `parse_frame`/`parse_message`, `split_mpub`, `parse_identify`/`identify_response`, name validation, nsqd's bounds and error codes |
| `actions.rs` | the `Protocol`/`Server` impls, four actions, `Answer`, the five events and their `answer_with` builders |

## Spec subset

After the four-byte magic `"  V2"`:

| Command | Answered by | Response |
|---|---|---|
| `IDENTIFY` + JSON | NetGet | `OK`, or with `feature_negotiation` the JSON nsqd sends (`max_rdy_count` 2500, `tls_v1`/`deflate`/`snappy`/`auth_required` false). `heartbeat_interval` −1 disables heartbeats, 0 keeps the server's, 1000–60000 ms is honoured, anything else `E_BAD_BODY`. Twice → `E_INVALID` |
| `PUB`, `MPUB`, `DPUB` | the model → `nsq_publish {command, topic, messages, message_count, total_bytes, defer_ms?, answer_with}` | `OK`, or the model's error |
| `SUB topic channel` | the model → `nsq_subscribe {topic, channel, ephemeral, client_id, user_agent, answer_with}` | `OK`, or the model's error. Twice, or after CLS → `E_INVALID` |
| `RDY n` | NetGet sets the count and sends waiting messages; then, if there is room, the model → `nsq_ready {count, topic, channel, ready, in_flight, pending, can_deliver, answer_with}` | messages, or nothing. Before SUB → `E_INVALID`; after CLS ignored |
| `FIN id` | NetGet (unknown id → `E_FIN_FAILED`, not fatal), sends the next waiting message, then the model → `nsq_finish {message_id, …}` | messages, or nothing |
| `REQ id timeout` | NetGet (unknown → `E_REQ_FAILED`), then the model → `nsq_requeue {message_id, timeout_ms, attempts, body, …}` | a redelivery, or nothing |
| `TOUCH id` | NetGet | nothing, or `E_TOUCH_FAILED` for an id not in flight |
| `NOP` | NetGet | nothing |
| `CLS` | NetGet | `CLOSE_WAIT`; the pending queue is dropped and nothing more is delivered. Before SUB → `E_INVALID` (as nsqd) |
| `AUTH` + body | NetGet | `E_AUTH_DISABLED` |
| anything else, bad names, bad counts | NetGet | nsqd's own code and text (`E_INVALID`, `E_BAD_TOPIC`, `E_BAD_CHANNEL`, `E_BAD_BODY`, `E_BAD_MESSAGE`) |

Every error except `E_FIN_FAILED`, `E_REQ_FAILED` and `E_TOUCH_FAILED` closes the connection,
which is nsqd's `FatalClientErr`/`ClientErr` split. A wrong magic gets `E_BAD_PROTOCOL` and the
close (`decision=fail_closed_bad_magic`). Topic and channel names follow nsqd:
`^[.a-zA-Z0-9_-]+(#ephemeral)?$`, 1–64 characters.

**Heartbeats** are `_heartbeat_` response frames on the negotiated interval (default 30 s,
`heartbeat_interval_secs`), written while the loop waits for a command **and** while the model
thinks — go-nsq times its reads out at twice the interval.

**What NetGet keeps**, per connection only: the subscription, RDY count, the messages in flight
(id → body and attempts, for FIN/REQ/TOUCH and a requeue event's body), and the queue of messages
the model chose that RDY has not yet let through. **NetGet is not a queue**: nothing published is
stored or routed to other connections. A consumer receives what the model delivers on its own
connection's events, so a message published after a consumer's last event reaches it only when
something raises another event (a FIN, a REQ, a new RDY) or an operator delivers it with
`[ message ]`.

Not implemented: in-flight timeouts (a message never FINed stays in flight; nsqd would requeue it
after `msg_timeout`), deferred delivery (`DPUB`'s `defer_ms` is shown to the model, not enforced),
TLS, snappy and deflate (negotiated off), AUTH, sampling, and nsqd's HTTP API.

## What the model sees and controls

| Action | Effect |
|---|---|
| `send_nsq_ok` | `OK` to a publish or subscription |
| `send_nsq_error {code, message}` | an error frame; `code` must be one of nsqd's, `message` is reduced to one line of at most 256 bytes |
| `deliver_nsq_messages {messages: [{body, attempts?}]}` | queued in order, sent while `in_flight < RDY`; ids (16 hex digits, per-server counter) and timestamps (ns) are NetGet's |
| `close_connection` | close after the answer |

A publish and a subscription take exactly one reply: the first `send_nsq_ok` or `send_nsq_error`
in the model's order is sent, the rest ignored. On RDY, FIN and REQ an `OK` is ignored (nsqd sends
none there, and go-nsq would read it as an answer to something else).

`answer_with` names, per request, the one answer it takes and where a delivered body comes from:
"exactly one action", the refusal code for that command, "at most N message(s) … not delivered
before … word for word … no action at all when none are waiting". The examples are placeholders
(`<message body>`, `<why>`); `tests/server/nsq/answer_with_test.rs` pins both.

## Failure behaviour

`FailureMode::Answers`.

| Situation | Wire | Log |
|---|---|---|
| backend failure on a publish | `E_PUB_FAILED` / `E_MPUB_FAILED` / `E_DPUB_FAILED` `<CMD> failed: broker backend unavailable` (or `… at capacity`), close | `decision=fail_closed_llm_error category=…` |
| backend failure on a subscription | `E_INVALID SUB failed: broker backend …`, close | same |
| backend failure on RDY/FIN/REQ | nothing (these take no reply) | same |
| the model answers a publish or subscription with nothing | `E_<CMD>_FAILED <CMD> failed` / `E_INVALID SUB failed`, close | `decision=model_silent` |
| the model refuses | its error frame | `decision=model_reject code=…` |
| the model accepts / delivers | `OK` / messages | `decision=model_answer` |
| the model delivers nothing on RDY/FIN/REQ | nothing | `decision=model_silent` |
| deliveries to an unsubscribed or closing connection | dropped | `decision=fail_closed_not_subscribed` |
| deliveries past the pending bound | dropped | `decision=fail_closed_pending_full` |

No error text reaches the peer. **Every close lingers** (2 s / 64 KiB), so an error frame written
before closing over unread input is not destroyed by an RST.

## Bounds

| Bound | Value | Override | Why |
|---|---|---|---|
| `MAX_MSG_SIZE` | 1 MiB | — | nsqd's `--max-msg-size` default. PUB/DPUB bodies and each MPUB message; judged from the size field before the body is read or allocated. `E_BAD_MESSAGE`, close, `decision=fail_closed_too_large`. |
| `MAX_BODY_SIZE` (= `max_inbound_bytes`) | 5 MiB | — | nsqd's `--max-body-size` default, for IDENTIFY, MPUB and AUTH bodies. `E_BAD_BODY`, close. |
| `MAX_MPUB_MESSAGES` | `(5 MiB − 4) / 5` | — | nsqd's own count bound; judged from the MPUB's eight leading bytes. `decision=fail_closed_mpub_count`. |
| `MAX_LINE` | 1024 incl. LF | — | No command is near it. Over it: `E_INVALID command line too long`, close. |
| `MAX_RDY_COUNT` | 2500 | — | nsqd's `--max-rdy-count`; `E_INVALID RDY count … out of range`. |
| `FIRST_BYTE_TIMEOUT` | 300 s | `first_byte_timeout_secs` | for the magic; the `manual` window for a NetGet TCP client parked on its operator. |
| heartbeat silence | 2 × interval | `heartbeat_interval_secs` / IDENTIFY | nsqd's rule. `decision=fail_closed_idle`. |
| `IDLE_TIMEOUT` | 300 s | `idle_timeout_secs` | silence for a client that disabled heartbeats. |
| `MAX_PENDING_MESSAGES` / `MAX_PENDING_BYTES` | 1000 / 16 MiB | — | deliveries waiting for RDY on one connection. |
| `MAX_CONNECTIONS` | 256 (house default) | — | the peer past the cap is closed with no bytes. |

Silence is measured between commands, from the later of the last byte read and the last answer
written, so a publish parked for a human is never closed for the time the human took.

## Peer handle

Registered at connect, and served by the session loop itself rather than
`peer_support::spawn_peer_command_task`, so an injected `deliver_nsq_messages` goes through the
same queue, RDY limit and id assignment as the model's. `close_connection` ends the connection.

## Wireshark

No NSQ dissector exists in this build (`tshark -G protocols` lists nothing matching nsq), so the
table maps `nsq` to plain TCP; "Follow TCP Stream" shows the command lines.

## Maturity

Beta. Evidence: `tests/server/nsq/real_client_test.rs` drives
the NSQ project's own `to_nsq` and `nsq_tail` (go-nsq; not linked; the server uses no NSQ
library): each stdin line is one PUB the handler accepted, a refusal makes `to_nsq` exit non-zero
naming `E_PUB_FAILED`, `nsq_tail` prints exactly the bodies delivered, and a mocked model carries
`to_nsq`'s lines to `nsq_tail`. It fails, never skips, without the binaries; CI's
`registry-audit` unpacks the release tarball and runs it. The `nsq_frame` fuzz target has oversize
and JSON-depth-bomb seeds and ran 180 s clean (75,018 executions; without AddressSanitizer, whose
runtime deadlocks at start-up on this macOS). Promoted after the whole suite (35 tests, with `otlp`'s 24
alongside) passed three consecutive runs at `--test-threads=100` and
`scripts/beta_evidence_table.py --check` stayed green with `to_nsq` and `nsq_tail` as the peers.

What Stable would need: a second independent client (pynsq, or nsqio/go-nsq's own `nsq_to_file`
counts as the same library), a pcap oracle (Wireshark has no dissector), in-flight timeouts.
