# MQTT-SN tests

Peers: `python3 tests/server/mqtt_sn/install_peers.py ROOT` builds mqtt-sn-tools (`make`) and the
Paho MQTT-SN Gateway (cmake, UDP) from hash-pinned source tarballs and prints
`NETGET_MQTTSN_PUB`, `NETGET_MQTTSN_SUB` and `NETGET_MQTTSN_GATEWAY`. Mosquitto and its clients
come from the system. `tests/helpers/mqtt_sn.rs` holds the gateway policy (refuse "intruder" and
admin/ publishes; answer a hold/# subscription with a greeting).

- `peer_test.rs` — mqtt-sn-tools against NetGet's gateway: wildcard and short-topic subscribers
  receive QoS 1, 0 and -1 (predefined id, no connection) publishes and one sent through
  forwarder encapsulation; an admin/ publish is refused and never reaches its subscriber; the
  client "intruder" gets CONNACK 0x03.
- `wire_test.rs` — every packet type round-trips; filter matching; NetGet's client against
  NetGet's gateway: QoS 2 both ways, a handler greeting, a lost client's will, a sleeping
  client's held message on wake, a peer-handle injection, and raw-socket refusals (GWINFO,
  publish without a session, refused client, unknown topic id, invalid filter).

`tests/client/mqtt_sn/peer_test.rs` — NetGet's client through the Paho gateway and Mosquitto.
