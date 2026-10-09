# LwM2M client tests

`peer_test.rs` runs the Eclipse Leshan 2.0.0-M15 server demo (independent, unchanged) on
loopback ports and drives it through its REST API: NetGet's device appears in Leshan's client
list with its objects, answers reads in text and SenML JSON, accepts a write that reads back,
an execute and a 4.04, and a notification it sends for Leshan's observation shows up in Leshan's
event stream (`Accept: text/event-stream` is required). On disconnect it deregisters and Leshan
drops it.
