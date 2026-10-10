# Apache Pulsar broker (server)

The Pulsar binary protocol, well-known port 6650 (Pulsar's own default; IANA assigns none).
`wire.rs` is shared with the client role: the protobuf commands as hand-declared prost
messages (field numbers and proto2 `required` labels from upstream `PulsarApi.proto`, so a
zero-valued required field — producer 0, request 0 — is still written), framing, the payload
section (magic `0x0e01`, CRC-32C over metadata and payload, `MessageMetadata`) and batch
entries (`SingleMessageMetadata`). No `protoc`.

## What Rust owns

- **CONNECT** → CONNECTED (`NetGet/<version>`, protocol min(client, 19), max message size
  `MAX_FRAME`). Anything before CONNECT ends the connection. Authentication data is accepted
  and not checked (the log says so).
- **PARTITIONED_METADATA** → 0 partitions; **LOOKUP** → Connect to this broker, at the address
  the client reached it on, authoritative. Topic names are normalised
  (`t` → `persistent://public/default/t`); a bad one is `InvalidTopicName`.
- **Producers**: names are assigned (`netget-<n>`) when the client gives none; encrypted
  producers are refused. PRODUCER_SUCCESS always carries an empty `schema_version` — the Java
  client reads it unconditionally and drops the connection without it.
- **SEND**: the checksum is verified (mismatch → `ChecksumError`, nothing reaches the model);
  compressed, encrypted and chunked payloads are refused; a batch is split into its messages.
- **Delivery** (`broker.rs`): topics hold subscriptions; a subscription keeps a backlog
  (`MAX_BACKLOG`, oldest dropped past it) and its consumers, each with permits from FLOW and its
  unacknowledged messages (`MAX_UNACKED`). Exclusive and Failover deliver to the first consumer,
  Shared and Key_Shared round-robin. ACK (individual or cumulative; an ACK carrying a request id
  is answered), REDELIVER_UNACKNOWLEDGED_MESSAGES, CLOSE_CONSUMER and a closed connection put
  messages back at the front. UNSUBSCRIBE deletes a subscription with one consumer. A new
  subscription starts at the latest message whatever position it asks for (nothing is kept
  for subscriptions that do not exist yet). GET_LAST_MESSAGE_ID is answered; GET_SCHEMA says
  there are no schemas; PING → PONG; other commands are ignored.

## What the model decides

- `pulsar_producer {topic, producer_name, access_mode, remote_addr}` and `pulsar_subscribe
  {topic, subscription, sub_type, consumer_name, remote_addr}`: `pulsar_accept` or
  `pulsar_reject {message}` (→ `AuthorizationError`).
- `pulsar_message {topic, producer_name, sequence_id, payload, encoding, properties, key,
  event_time, remote_addr}`: one event per message of a batch; the batch is stored only if all
  are accepted. `payload` is UTF-8 text, or hex with `encoding: "hex"` when it is not printable.
- Any of them may add `pulsar_publish {topic, payload, encoding?, properties?, key?}`: a message
  from producer `netget`, delivered like any other, after the answer it accompanies.

### Refusing a message

Pulsar has no refusal every client honours. The Java client fails a send answered with
`NotAllowedError`. The C++ library — and the Python and Node clients built on it — treats every
SendError except `ChecksumError` as a broken connection: it reconnects at once and resends the
message, forever (measured: ~740 reconnects in a few seconds). So `refusal_code` answers
clients whose version starts `Pulsar-CPP` with `ChecksumError`, which they fail the send with,
and everyone else with `NotAllowedError`; the model's reason is the message text either way
(the C++ library logs it). A refused send is remembered (`MAX_REFUSALS`), so a resend is
refused again without asking the model.

## Failure

A failed model call, invalid reply, silence or two verdicts refuse: producers and
subscriptions with `ServiceNotReady`, messages as above; the text is a category, never the
error. Decisions are logged `decision=model_answer|model_reject|model_silent|fail_closed_*`, and
`remembered_refusal` for a resend.

## Bounds

Frames `MAX_FRAME` (5 MiB, the connection ends past it), commands `MAX_COMMAND` (64 KiB), batches
`MAX_BATCH` (1000), `MAX_HANDLES` (256) producers and consumers per connection, `MAX_TOPICS`,
idle `idle_timeout_secs` (default 300; clients ping every 30 s), `DEFAULT_MAX_CONNECTIONS`.

## Not implemented

Persistence, partitioned topics, schemas, compression, encryption, chunking, transactions,
authentication, TLS, seek, topic listing, consumer stats.
