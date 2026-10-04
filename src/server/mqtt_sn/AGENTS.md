# MQTT-SN gateway — Experimental

MQTT-SN 1.2 over UDP. `packet.rs` is the codec: the 1- or 3-byte length header, every message
type, flags, normal / predefined / short topic ids, forwarder encapsulation, and MQTT topic
filter matching. `mod.rs` is the gateway, and the gateway is the broker: there is no MQTT broker
behind it.

One task owns all state. Datagrams, handler answers, peer-handle commands and a 200 ms timer
arrive on it in turn; handler calls run as their own tasks and report back, and a client's later
datagrams (other than acknowledgements and pings) wait in a backlog of 64 while its call is out.

Rust owns:
- Sessions keyed by address and forwarder node id (replies to a forwarded client are
  encapsulated again), at most 256. CONNECT with the will flag runs WILLTOPICREQ/WILLMSGREQ
  first. A client id that reconnects from a new address takes its session along; without clean
  session, subscriptions and held messages survive a disconnect.
- Topic ids: one gateway-wide registry (10 000 ids) handed out on REGISTER and SUBSCRIBE;
  predefined ids from the `predefined_topics` startup parameter; two-character names as short
  topics. Deliveries use the predefined id or short name where one applies, otherwise the
  registry id, sending REGISTER first when the client has not learned it.
- QoS: inbound QoS -1 (predefined or short topics, no session), 0, 1 (PUBACK) and 2 (PUBREC,
  PUBREL, PUBCOMP, duplicates answered without asking again); outbound at the lower of the
  subscription's and the message's QoS, one outstanding delivery per client, retried every 5 s,
  3 times, with DUP set.
- Keep-alive: a client silent for 1.5 x its duration is lost and its will published.
  DISCONNECT with a duration puts a client to sleep; messages are held (100 per client) until a
  PINGREQ carrying its client id, delivered, then PINGRESP and back to sleep; a client that
  oversleeps 1.5 x its duration is lost.
- SEARCHGW is answered with GWINFO (`gateway_id`); unknown topic ids with PUBACK
  invalid_topic_id; a PUBLISH without a session with DISCONNECT.

The handler answers `mqttsn_connect`, `mqttsn_message` and `mqttsn_subscribe` with
`mqttsn_accept` (a subscription may lower the QoS) or `mqttsn_reject` (congestion,
invalid_topic_id, not_supported), plus any number of `mqttsn_publish` (to all subscribers, or one
client_id). No answer is congestion: CONNACK, PUBACK or SUBACK carrying it. Retained messages are
not stored; answer a subscription with `mqttsn_publish` to deliver one. A session's peer handle
accepts `mqttsn_publish` and `disconnect`.

Not implemented: ADVERTISE broadcasts, the aggregating and transparent gateway modes, DTLS.
