# TR-069 device tests

`real_server_test.rs` runs MongoDB 8.0.4, genieacs-cwmp and genieacs-nbi (GenieACS 1.2.16, one
worker each, every process in its own group and killed whole on drop) from
`tests/server/tr069/install_peers.py`.

`netget_is_a_device_genieacs_manages`: NetGet informs with `0 BOOTSTRAP` and `1 BOOT`;
GenieACS registers `4E4554-NetGetCPE-NETGET0042` with its connection-request URL. Then tasks
posted to GenieACS's NBI with `connection_request`: GenieACS GETs NetGet's URL, NetGet opens a
`6 CONNECTION REQUEST` session, and GenieACS sends what it needs (it discovers the data model
with GetParameterNames before reading). A python chain answers from a three-parameter table.
The NBI answers 200 (task done) and its device document holds the value read (`2.1`) and the
value written (`600`, sent as `xsd:unsignedInt`).

Mutation-checked: dropping the model's actions fails the test. No LLM calls.
