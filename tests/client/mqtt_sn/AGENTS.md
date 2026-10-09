# MQTT-SN client tests

`peer_test.rs` runs Mosquitto and, in front of it, the Paho MQTT-SN Gateway (independent,
unchanged; readiness is a CONNECT it answers, since its stdout is buffered on a pipe). NetGet's
client publishes with QoS 1, 2 and 0 to topics `mosquitto_sub` prints, subscribes and receives
what `mosquitto_pub` sends, and while asleep has a message held by the Paho gateway and delivered
on wake. Needs `NETGET_MQTTSN_GATEWAY` from `tests/server/mqtt_sn/install_peers.py` and
`mosquitto`, `mosquitto_sub`, `mosquitto_pub` on the path.
