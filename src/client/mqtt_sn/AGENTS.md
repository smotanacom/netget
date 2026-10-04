# MQTT-SN client — Experimental

Uses `src/server/mqtt_sn/packet.rs`. Connects (with a will exchange when `will_topic` is set)
and reports `mqttsn_connected`. One operation is outstanding at a time, retried every 5 s, 3
times; later actions queue. `mqttsn_register`, `mqttsn_publish` (registers a name first when
needed; QoS -1 only on short or predefined topics; QoS 1 and 2 exchanges), `mqttsn_subscribe`,
`mqttsn_unsubscribe`, `mqttsn_sleep` (DISCONNECT with a duration), `mqttsn_wake` (PINGREQ with
the client id: held messages, then PINGRESP), `mqttsn_connect` (become active again),
`mqttsn_ping` and `disconnect`. Each reports `mqttsn_result` with the return code (`sent` for
QoS 0 and -1, `timeout` when the gateway never answered). Deliveries are acknowledged
(PUBACK, PUBREC/PUBCOMP; REGISTER from the gateway gets REGACK) and reported as
`mqttsn_message_received`. While awake it pings at three quarters of its keep-alive.
Not implemented: gateway discovery, forwarder encapsulation.
