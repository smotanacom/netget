# Fluent Forward emitter

`send_forward_batch` takes tag, entries with typed timestamps/JSON records, mode message/
forward/packed/compressed_packed, and require_ack. It validates the complete batch before
writing. require_ack creates an internal random 128-bit chunk token and a `forward_ack` event
only after matching receipt. Chunk bytes stay inside the transport. Command return Sent confirms
local transport acceptance; ACK is an event and does not promise durable storage. No replay/retry.
Packed and compressed packed sends validate their inner stream's depth/value limits as well
as the outer frame before emission. ACK receipt after its deadline also fails closed.

One owned TCP task handles framing, commands, ACK correlation/deadlines and event handlers.
Command handle registers before `forward_connected`. A parked connect or ACK handler does
not block injected sends/disconnect or ACK reading. TCP EOF, malformed/mismatched/repeated
ACK, secure-forward greeting, timeout, queue overflow or transport error closes the session.
Standard client memory is refreshed for each connected/ACK dispatch; shared common actions
and model memory_updates are applied before subsequent event handling.

Bounds: 256KiB encoded/decompressed stream, 256 records, depth32/value16384 limits;
32 pending ACKs, 32 queued response events, 8 follow-up depth and 32 protocol actions per handler.
Connect/write/ACK deadlines are 10s. Pending ACKs/events are transport state, never a domain
store. Empty instruction without configured handlers records events without model calls.
Handler errors are logged; failed outbound validation rejects atomically, preserving transport.
Stop/disconnect drops pending handler and socket; no detached tasks.

Experimental. Official Fluentd1.19.4 in_forward decodes four carriers, EventTime, Unicode and
records, and supplies real ACKs. No secure-forward/TLS, UDP heartbeat, JSON framing, storage,
deduplication, automatic retry or complete Fluentd platform claim.
