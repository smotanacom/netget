# AMQP 1.0 container — Experimental

Separate from the AMQP 0-9-1 `amqp` feature. `types.rs` is the type system (every primitive
encoding, lists, maps, arrays with one constructor, described types; depth 32, 100 000 values,
compound sizes checked against their contents). `frame.rs` is the transport: headers, frames
(empty frames are keep-alives), performative codes. `message.rs` maps messages to JSON:
properties, application properties, message annotations, and the body as amqp-value,
amqp-sequence or data (text when UTF-8, else `binary_length`).

Rust owns:
- Headers and SASL (PLAIN and ANONYMOUS; other mechanisms fail with code 1), or no SASL layer
  unless `require_sasl`. Open with max-frame-size 1 MiB, channel-max 15, `idle_timeout_secs`;
  keep-alive frames at half the client's idle-time-out; a silent client is closed.
- Sessions (begin/end), links (attach/detach, handle-max 63), refusal as a null terminus plus
  detach with the error (a client's detach of a refused link is not answered twice), link credit
  (100 granted to publishers, topped up at half; a consumer's credit from its flow per part 2,
  2.6.7, `echo` answered), multi-frame transfers both ways (1 MiB per message), dispositions with
  accepted / rejected / released outcomes, pre-settled transfers.
- Addresses are topics: an accepted message is relayed to the consumers attached to its address;
  one without credit holds up to 1000.

The handler answers `amqp1_connect`, `amqp1_attach` (publish or consume), `amqp1_message` (one
outcome — `amqp1_accept`, `amqp1_reject` with condition and description, `amqp1_release` — and
any `amqp1_send`) and `amqp1_credit` (a consumer with credit and nothing waiting: `amqp1_send`
with no address answers that link, or `amqp1_ignore`). No answer: SASL code 2, a closed
connection, a detached link or a rejected message with amqp:internal-error. A connection's peer
handle accepts `amqp1_send` (with an address) and `disconnect`.

Not implemented: transactions, link recovery and resumption, filters, durable storage, AMQP over
WebSocket, TLS.
